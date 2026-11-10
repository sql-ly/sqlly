#!/usr/bin/env bash
# Installs SQLLY's desktop entry and icons into the current user's XDG data
# directory. Unlike a macOS .app or a Windows .exe, a Linux binary carries no
# icon of its own — the desktop matches the window's app_id against an
# installed .desktop file of the same name, and only then resolves Icon= via
# the hicolor theme. Until these files are copied out of the tarball, there is
# nothing to match and the window shows a generic placeholder.
#
# Run from the unpacked tarball directory. Re-runnable; installs nothing
# system-wide and needs no root.
set -euo pipefail

APP_NAME="SQLLY"
# Must match `about::APP_ID` in the Rust source — this is the string the
# compositor reports for the window, and the basename it looks for here.
APP_ID="io.zebrasoft.sqlly"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="$HERE/$APP_NAME"
DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"

if [ ! -x "$BIN" ]; then
  echo "error: $APP_NAME binary not found next to this script (looked at $BIN)." >&2
  exit 1
fi

# Icons first: a .desktop entry pointing at an icon that isn't there yet shows
# a placeholder that some desktops then cache.
installed=0
for size in 16 32 48 64 128 256 512; do
  src="$HERE/icons/hicolor/${size}x${size}/apps/$APP_ID.png"
  [ -f "$src" ] || continue
  dest_dir="$DATA_HOME/icons/hicolor/${size}x${size}/apps"
  mkdir -p "$dest_dir"
  cp "$src" "$dest_dir/$APP_ID.png"
  installed=$((installed + 1))
done

if [ "$installed" -eq 0 ]; then
  echo "error: no icons found under $HERE/icons/hicolor — is this a complete tarball?" >&2
  exit 1
fi

# Rewrite Exec= to wherever the user actually unpacked us. The staged .desktop
# ships a bare "Exec=SQLLY", which only resolves if the binary is on PATH.
DESKTOP_DIR="$DATA_HOME/applications"
mkdir -p "$DESKTOP_DIR"
sed "s|^Exec=.*|Exec=$BIN %u|" "$HERE/$APP_ID.desktop" > "$DESKTOP_DIR/$APP_ID.desktop"

# Refresh the caches. Both are best-effort: absent on minimal installs, and
# most desktops pick the files up on their own within a few seconds anyway.
update-desktop-database "$DESKTOP_DIR" 2>/dev/null || true
xdg-mime default "$APP_ID.desktop" x-scheme-handler/sqlly 2>/dev/null || true
gtk-update-icon-cache -f -t "$DATA_HOME/icons/hicolor" 2>/dev/null || true

echo "Installed $APP_NAME desktop entry and $installed icons under $DATA_HOME."
echo "Launch from your application menu, or run $BIN directly."
