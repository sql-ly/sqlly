#!/usr/bin/env bash
set -euo pipefail

# ─────────────────────────────────────────────────────────────────────────────
# Sign + notarize + staple a pre-built SQLLY.app, then build a notarized DMG.
#
# Does NOT rebuild the binary or re-bundle the .app — it operates on the
# dist/SQLLY.app produced by build-app.sh. The pipeline calls build-app.sh
# once (cargo build + bundle), then this script for the signing chain.
#
# Required env (provided by the pipeline):
#   TEAM_ID     — Apple Developer Team ID
#   BUNDLE_ID   — app bundle identifier (e.g. app.sqlly)
#   APP_VERSION — version string (informational)
#
# Prerequisites (handled by earlier pipeline steps):
#   * Developer ID Application .p12 imported into a temp keychain that is the
#     default keychain for this run.
#   * notarytool credentials stored under the keychain profile "sqlly notarize".
#
# Flow:
#   1. Resolve the Developer ID Application signing identity from the keychain.
#   2. codesign the .app (deep).
#   3. Notarize the .app via notarytool (submit zip → wait → staple).
#   4. Build the DMG from the signed + stapled .app.
#   5. Notarize the DMG via notarytool (submit → wait → staple).
#   6. Verify both staples.
# ─────────────────────────────────────────────────────────────────────────────

PACKAGE_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_NAME="${APP_NAME:-SQLLY}"
DIST_DIR="${DIST_DIR:-$PACKAGE_ROOT/dist}"
APP_BUNDLE="$DIST_DIR/$APP_NAME.app"
DMG_PATH="$DIST_DIR/$APP_NAME.dmg"
NOTARY_PROFILE="${NOTARY_PROFILE:-sqlly notarize}"

step() { echo ""; echo "=== [$(date '+%H:%M:%S')] $1 ==="; }

# Submit a file to Apple's notary service and block until Apple finishes
# scanning it. Release builds use the maintainers' notarize wrapper when it is
# present (it rides out transient network errors and prints Apple's log on a
# rejection); otherwise this calls notarytool directly.
NOTARIZE_SCRIPT="$PACKAGE_ROOT/../../../pipelines/scripts/notarize.sh"
notarize_and_wait() {
  local path="$1"
  step "Uploading $(basename "$path") to Apple notary service (this waits for Apple's scan)"
  if [ -x "$NOTARIZE_SCRIPT" ]; then
    "$NOTARIZE_SCRIPT" "$path" "$NOTARY_PROFILE" || exit 1
  else
    xcrun notarytool submit "$path" --keychain-profile "$NOTARY_PROFILE" --wait \
      | tee /dev/stderr | grep -q 'status: Accepted' || exit 1
  fi
}

# ── Verify the pre-built app exists ───────────────────────────────────
if [ ! -d "$APP_BUNDLE" ]; then
  echo "error: $APP_BUNDLE not found — run build-app.sh first." >&2
  exit 1
fi

# ── Resolve the Developer ID Application identity ─────────────────────
SIGN_IDENTITY="${SIGN_IDENTITY:-}"
if [ -z "$SIGN_IDENTITY" ]; then
  # Extract the 40-hex SHA-1 from the find-identity line. Not sed with \s:
  # BSD sed (macOS) has no \s in ERE, so that pattern silently no-ops and
  # passes the whole line to codesign, which fails with "no identity found".
  SIGN_IDENTITY="$(security find-identity -v -p codesigning \
    | grep "Developer ID Application" \
    | head -1 \
    | grep -oE '[0-9A-F]{40}' \
    | head -1)"
  if [ -z "$SIGN_IDENTITY" ]; then
    echo "error: no Developer ID Application identity found in keychain." >&2
    exit 1
  fi
fi
echo "Using signing identity: $SIGN_IDENTITY"

# ── 1. Sign the .app ──────────────────────────────────────────────────
step "Signing $APP_BUNDLE with identity $SIGN_IDENTITY"
codesign --force --deep --options runtime --timestamp --verbose \
  --sign "$SIGN_IDENTITY" "$APP_BUNDLE"
codesign --verify --strict --verbose=2 "$APP_BUNDLE" 2>&1

# ── 2. Notarize the .app ──────────────────────────────────────────────
ZIP_PATH="$DIST_DIR/${APP_NAME}-notarize.zip"
step "Creating notarization archive: $ZIP_PATH"
ditto -c -k --keepParent "$APP_BUNDLE" "$ZIP_PATH"
ls -lh "$ZIP_PATH"

notarize_and_wait "$ZIP_PATH"

step "Stapling notarization ticket to .app"
xcrun stapler staple "$APP_BUNDLE"
xcrun stapler validate "$APP_BUNDLE"

rm -f "$ZIP_PATH"

# ── 3. Build DMG from the signed + stapled .app ───────────────────────
step "Building DMG from signed app: $DMG_PATH"
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

# ── 4. Sign + notarize + staple the DMG ───────────────────────────────
step "Signing DMG"
codesign --force --timestamp --verbose --sign "$SIGN_IDENTITY" "$DMG_PATH"

notarize_and_wait "$DMG_PATH"

step "Stapling notarization ticket to DMG"
xcrun stapler staple "$DMG_PATH"
xcrun stapler validate "$DMG_PATH"

# ── 5. Gatekeeper assessment — what an end user's Mac will decide ─────
step "Gatekeeper assessment"
spctl --assess --type exec --verbose=2 "$APP_BUNDLE"
spctl --assess --type open --context context:primary-signature --verbose=2 "$DMG_PATH"

# ── Done ──────────────────────────────────────────────────────────────
echo ""
echo "=== Distribution artifacts ready ==="
echo "  App: $APP_BUNDLE (signed + notarized + stapled)"
echo "  DMG: $DMG_PATH (signed + notarized + stapled)"
