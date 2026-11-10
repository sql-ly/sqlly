#!/usr/bin/env bash
set -euo pipefail

# ─────────────────────────────────────────────────────────────────────────────
# Build the GPUI SQLLY app for the current platform.
#
# macOS:  produces dist/SQLLY.app with Contents/{MacOS,Resources} + Info.plist,
#         AND dist/SQLLY.dmg (unsigned, ad-hoc signed).
#         Apple Silicon (arm64) preferred; x86_64 is allowed but not recommended.
# Linux:  produces dist/SQLLY (standalone binary) + dist/io.zebrasoft.sqlly.desktop
#         + dist/icons/hicolor/*/apps/io.zebrasoft.sqlly.png (freedesktop.org icon
#         theme) + dist/install.sh, which copies both into the user's XDG dirs.
# Windows: produces dist/SQLLY.exe (with embedded icon via build.rs + embed-resource).
#
# Usage:
#   ./scripts/build-app.sh                  # release build
#   CONFIGURATION=debug ./scripts/build-app.sh
#   CODESIGN_APP=1 ./scripts/build-app.sh   # ad-hoc sign (macOS only)
#   BUILD_DMG=0 ./scripts/build-app.sh      # skip DMG (macOS only, default: 1)
#   APP_NAME="SQLLY Beta" ./scripts/build-app.sh
#   CARGO_FEATURES=app-store ./scripts/build-app.sh   # extra cargo features
#   TARGET=x86_64-unknown-linux-gnu ./scripts/build-app.sh  # cross-compile
# ─────────────────────────────────────────────────────────────────────────────

APP_NAME="${APP_NAME:-SQLLY}"
# Linux desktop identity. Must match `about::APP_ID` in the Rust source: the
# desktop matches a window's app_id against this .desktop basename to find the
# icon, so the two drifting apart silently costs the icon.
APP_ID="${APP_ID:-io.zebrasoft.sqlly}"
CONFIGURATION="${CONFIGURATION:-release}"
PACKAGE_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO_ROOT="$(cd "$PACKAGE_ROOT/../../.." && pwd)"
WORKSPACE_ROOT="$REPO_ROOT/client"
DIST_DIR="${DIST_DIR:-$PACKAGE_ROOT/dist}"
CARGO_BIN_NAME="SQLLY"   # [[bin]] name in Cargo.toml
EXECUTABLE_NAME="SQLLY"       # name inside the bundle / final binary

# ── Determine the target triple ──────────────────────────────────────────────
if [ -n "${TARGET:-}" ]; then
  # Explicit cross-compile target
  CARGO_TARGET="$TARGET"
elif [ "$(uname -s)" = "Darwin" ]; then
  CARGO_TARGET="aarch64-apple-darwin"
  if [ "${APP_ARCH_OVERRIDE:-}" != "arm64" ] && [ -n "${APP_ARCH_OVERRIDE:-}" ]; then
    echo "error: x86_64 / Intel Macs are not supported. APP_ARCH_OVERRIDE='$APP_ARCH_OVERRIDE' is invalid." >&2
    echo "       SQLLY targets Apple Silicon (arm64) only." >&2
    exit 1
  fi
else
  # Let cargo pick the native host target
  CARGO_TARGET=""
fi

# ── Build the Rust binary ───────────────────────────────────────────────────
# CARGO_PROFILE_FLAG overrides the default `--release`/`--profile <name>` flag.
# Set it to e.g. `--profile release-ci` to use a non-release profile (the CI
# pipeline does this to use thin LTO for faster builds). When set, it takes
# precedence over CONFIGURATION.
if [ -n "${CARGO_PROFILE_FLAG:-}" ]; then
  CARGO_FLAG="$CARGO_PROFILE_FLAG"
  # cargo outputs to target/<triple>/<profile-name>/ for custom profiles, or
  # target/<triple>/release/ for --release. Derive the output dir from the flag.
  PROFILE_DIR="$(echo "$CARGO_PROFILE_FLAG" | sed -E 's/^--profile //')"
  if [ -z "$PROFILE_DIR" ]; then PROFILE_DIR="release"; fi
elif [ "$CONFIGURATION" = "debug" ]; then
  CARGO_FLAG=""
  PROFILE_DIR="debug"
else
  CARGO_FLAG="--release"
  PROFILE_DIR="release"
fi

# Honor CARGO_TARGET_DIR (CI sets it to a dir outside the checkout so
# `checkout: clean` can't wipe build artifacts); default to the in-tree dir.
TARGET_DIR="${CARGO_TARGET_DIR:-$WORKSPACE_ROOT/target}"

# Optional cargo features (e.g. CARGO_FEATURES=app-store from
# app-store/build-app-store.sh). Empty by default, so the normal build is
# unchanged.
FEATURE_ARGS=()
if [ -n "${CARGO_FEATURES:-}" ]; then
  FEATURE_ARGS=(--features "$CARGO_FEATURES")
  echo "Cargo features: $CARGO_FEATURES"
fi

if [ -n "$CARGO_TARGET" ]; then
  echo "Building $CARGO_BIN_NAME ($CARGO_FLAG) for $CARGO_TARGET ..."
  cargo build -p sqlly-gpui --target "$CARGO_TARGET" $CARGO_FLAG ${FEATURE_ARGS[@]+"${FEATURE_ARGS[@]}"} --manifest-path "$WORKSPACE_ROOT/Cargo.toml"
  BIN_DIR="$TARGET_DIR/$CARGO_TARGET/$PROFILE_DIR"
else
  echo "Building $CARGO_BIN_NAME ($CARGO_FLAG) for native target ..."
  cargo build -p sqlly-gpui $CARGO_FLAG ${FEATURE_ARGS[@]+"${FEATURE_ARGS[@]}"} --manifest-path "$WORKSPACE_ROOT/Cargo.toml"
  BIN_DIR="$TARGET_DIR/$PROFILE_DIR"
fi

BUILT_BIN="$BIN_DIR/$CARGO_BIN_NAME"
if [ ! -f "$BUILT_BIN" ]; then
  echo "error: built binary not found at $BUILT_BIN" >&2
  exit 1
fi

mkdir -p "$DIST_DIR"

# ── Platform-specific packaging ─────────────────────────────────────────────
OS_TAG="$(uname -s)"

if [ "$OS_TAG" = "Darwin" ]; then
  # ── macOS: .app bundle ────────────────────────────────────────────────────
  APP_BUNDLE="$DIST_DIR/$APP_NAME.app"
  rm -rf "$APP_BUNDLE"
  mkdir -p "$APP_BUNDLE/Contents/MacOS" "$APP_BUNDLE/Contents/Resources"

  cp "$BUILT_BIN" "$APP_BUNDLE/Contents/MacOS/$EXECUTABLE_NAME"
  chmod +x "$APP_BUNDLE/Contents/MacOS/$EXECUTABLE_NAME"

  # ── App icon (reuse the Swift app's icon if available) ────────────────────
  SWIFT_ICON="$PACKAGE_ROOT/../macos/Sources/SQLLY/Resources/Icons/AppIcon.icns"
  LOCAL_ICON="$PACKAGE_ROOT/resources/AppIcon.icns"
  if [ -f "$LOCAL_ICON" ]; then
    cp "$LOCAL_ICON" "$APP_BUNDLE/Contents/Resources/AppIcon.icns"
  elif [ -f "$SWIFT_ICON" ]; then
    cp "$SWIFT_ICON" "$APP_BUNDLE/Contents/Resources/AppIcon.icns"
  fi

  # ── Info.plist ────────────────────────────────────────────────────────────
  VERSION="${APP_VERSION:-$(grep -m1 '^version' "$WORKSPACE_ROOT/Cargo.toml" | sed 's/.*= *"\(.*\)"/\1/')}"
  PLIST_TEMPLATE="$PACKAGE_ROOT/resources/Info.plist"
  if [ -f "$PLIST_TEMPLATE" ]; then
    sed "s/\$APP_NAME/$APP_NAME/g; s/\$VERSION/$VERSION/g" "$PLIST_TEMPLATE" \
      > "$APP_BUNDLE/Contents/Info.plist"
  else
    cat > "$APP_BUNDLE/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key>
  <string>en</string>
  <key>CFBundleDisplayName</key>
  <string>$APP_NAME</string>
  <key>CFBundleExecutable</key>
  <string>$EXECUTABLE_NAME</string>
  <key>CFBundleIconFile</key>
  <string>AppIcon</string>
  <key>CFBundleIdentifier</key>
  <string>app.sqlly</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6</string>
  <key>CFBundleName</key>
  <string>$APP_NAME</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleURLTypes</key>
  <array>
    <dict>
      <key>CFBundleURLName</key>
      <string>sqlly auth callback</string>
      <key>CFBundleURLSchemes</key>
      <array>
        <string>sqlly-auth</string>
      </array>
    </dict>
    <dict>
      <key>CFBundleURLName</key>
      <string>SQLLY deep link</string>
      <key>CFBundleURLSchemes</key>
      <array>
        <string>sqlly</string>
      </array>
    </dict>
  </array>
  <key>CFBundleDocumentTypes</key>
  <array>
    <dict>
      <key>CFBundleTypeName</key>
      <string>SQLite database</string>
      <key>CFBundleTypeRole</key>
      <string>Viewer</string>
      <key>LSHandlerRank</key>
      <string>Alternate</string>
      <key>CFBundleTypeExtensions</key>
      <array>
        <string>sqlite</string>
        <string>db</string>
        <string>sqlite3</string>
      </array>
    </dict>
  </array>
  <key>CFBundleShortVersionString</key>
  <string>$VERSION</string>
  <key>CFBundleVersion</key>
  <string>1</string>
  <key>LSMinimumSystemVersion</key>
  <string>13.0</string>
  <key>NSLocalNetworkUsageDescription</key>
  <string>$APP_NAME connects to database servers on your local network, such as virtual machines, containers, and servers on your LAN.</string>
  <key>NSHighResolutionCapable</key>
  <true/>
</dict>
</plist>
PLIST
  fi

  plutil -lint "$APP_BUNDLE/Contents/Info.plist"

  # ── Guard: arm64 only (no x86_64 / universal) ─────────────────────────────
  if command -v lipo >/dev/null 2>&1; then
    archs="$(lipo -archs "$APP_BUNDLE/Contents/MacOS/$EXECUTABLE_NAME" 2>/dev/null || true)"
    if [ "$archs" != "arm64" ]; then
      echo "error: $EXECUTABLE_NAME has architectures '$archs', expected only 'arm64'" >&2
      echo "       x86_64 / Intel Macs are NOT supported." >&2
      exit 1
    fi
  fi

  # ── Guard: MacOS/ must contain exactly the GUI executable ─────────────────
  EXPECTED_BINS="$EXECUTABLE_NAME"
  ACTUAL_BINS="$(ls -1 "$APP_BUNDLE/Contents/MacOS" | sort)"
  if [ "$EXPECTED_BINS" != "$ACTUAL_BINS" ]; then
    echo "error: unexpected contents in $APP_BUNDLE/Contents/MacOS" >&2
    echo "       expected exactly: $EXPECTED_BINS" >&2
    echo "       found: $ACTUAL_BINS" >&2
    exit 1
  fi

  # ── Optional: ad-hoc code signing ─────────────────────────────────────────
  if [ "${CODESIGN_APP:-0}" = "1" ] && command -v codesign >/dev/null 2>&1; then
    codesign --force --deep --sign - "$APP_BUNDLE"
  fi

  # ── Build a distributable DMG ──────────────────────────────────────────────
  # Stages the .app + an /Applications symlink into a temp folder and creates a
  # UDZO (zlib-compressed) DMG. This is the UNSIGNED DMG — for distribution,
  # build-distributable-app.sh signs + notarizes + staples a separate DMG. Set
  # BUILD_DMG=0 to skip (e.g. when only the .app is needed for signing).
  BUILD_DMG="${BUILD_DMG:-1}"
  DMG_PATH="$DIST_DIR/$APP_NAME.dmg"
  if [ "$BUILD_DMG" = "1" ] && command -v hdiutil >/dev/null 2>&1; then
    echo "Building DMG: $DMG_PATH"
    rm -f "$DMG_PATH"
    DMG_STAGING="$(mktemp -d -t sqlly-dmg)"
    trap 'rm -rf "$DMG_STAGING"' EXIT
    cp -R "$APP_BUNDLE" "$DMG_STAGING/$APP_NAME.app"
    ln -s /Applications "$DMG_STAGING/Applications"
    hdiutil create -volname "$APP_NAME" \
      -srcfolder "$DMG_STAGING" \
      -ov -format UDZO \
      "$DMG_PATH"
    rm -rf "$DMG_STAGING"
    trap - EXIT
    echo "$DMG_PATH"
  fi

  echo "$APP_BUNDLE"

elif [ "$OS_TAG" = "Linux" ]; then
  # ── Linux: standalone binary + .desktop entry + hicolor icons ─────────────
  LINUX_BIN="$DIST_DIR/$EXECUTABLE_NAME"
  cp "$BUILT_BIN" "$LINUX_BIN"
  chmod +x "$LINUX_BIN"

  # ── .desktop file for desktop integration ─────────────────────────────────
  VERSION="${APP_VERSION:-$(grep -m1 '^version' "$WORKSPACE_ROOT/Cargo.toml" | sed 's/.*= *"\(.*\)"/\1/')}"
  DESKTOP_FILE="$DIST_DIR/$APP_ID.desktop"
  # Version= below is the freedesktop *spec* version, not ours — putting the
  # app version there makes the entry invalid (desktop-file-validate rejects it),
  # so ours goes in X-AppVersion.
  cat > "$DESKTOP_FILE" <<DESKTOP
[Desktop Entry]
Type=Application
Name=$APP_NAME
Comment=SQLLY database client
Exec=$LINUX_BIN
Icon=$APP_ID
Categories=Development;Database;
Terminal=false
Version=1.5
X-AppVersion=$VERSION
DESKTOP

  # ── hicolor icon theme for desktop integration ────────────────────────────
  # Package PNG icons in the freedesktop.org hicolor layout so desktop
  # environments can find them via the standard Icon=$APP_ID lookup.
  ICONS_SRC="$PACKAGE_ROOT/resources/icons"
  for size in 16 32 48 64 128 256 512; do
    ICON_DIR="$DIST_DIR/icons/hicolor/${size}x${size}/apps"
    mkdir -p "$ICON_DIR"
    cp "$ICONS_SRC/app-icon-${size}.png" "$ICON_DIR/$APP_ID.png"
  done

  # ── installer ─────────────────────────────────────────────────────────────
  # Staging the icons is not enough: they only take effect once copied into
  # the user's XDG data dir, where the window's app_id can be matched to them.
  cp "$PACKAGE_ROOT/scripts/linux-install.sh" "$DIST_DIR/install.sh"
  chmod +x "$DIST_DIR/install.sh"

  echo "$LINUX_BIN"

else
  # ── Windows / other: just copy the binary ─────────────────────────────────
  OUTPUT_BIN="$DIST_DIR/$EXECUTABLE_NAME"
  cp "$BUILT_BIN" "$OUTPUT_BIN"
  chmod +x "$OUTPUT_BIN" 2>/dev/null || true

  echo "$OUTPUT_BIN"
fi
