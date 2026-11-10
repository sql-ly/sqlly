#!/usr/bin/env bash
# Regenerates the Linux hicolor PNG set from the master SVG.
#
# These PNGs are the only copy of the app art that isn't derived at build time
# — macOS reads AppIcon.icns and Windows compiles sqlly.ico into the .exe, so
# a rebrand that misses this directory leaves Linux showing the old logo with
# nothing failing anywhere. Re-run this whenever the master SVG changes.
#
# Usage: ./scripts/generate-linux-icons.sh [path/to/master.svg]
set -euo pipefail

PACKAGE_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO_ROOT="$(cd "$PACKAGE_ROOT/../../.." && pwd)"
SVG="${1:-$REPO_ROOT/assets/sqlly2.svg}"
OUT_DIR="$PACKAGE_ROOT/resources/icons"
SIZES=(16 32 48 64 128 256 512)

if [ ! -f "$SVG" ]; then
  echo "error: master SVG not found at $SVG" >&2
  exit 1
fi

# Render each size from the vector rather than downscaling one raster, so the
# small sizes stay legible. ImageMagick is deliberately not a fallback: without
# an rsvg delegate it silently swaps in its own renderer and mangles the art.
render() {
  local size="$1" out="$2"
  if command -v rsvg-convert >/dev/null 2>&1; then
    rsvg-convert -w "$size" -h "$size" -o "$out" "$SVG"
  else
    python3 - "$SVG" "$size" "$out" <<'PY'
import sys
try:
    import gi
    gi.require_version("GdkPixbuf", "2.0")
    from gi.repository import GdkPixbuf
except Exception as e:
    sys.exit(f"error: need rsvg-convert or python3-gi with the librsvg loader ({e})")
svg, size, out = sys.argv[1], int(sys.argv[2]), sys.argv[3]
pb = GdkPixbuf.Pixbuf.new_from_file_at_scale(svg, size, size, True)
pb.savev(out, "png", [], [])
PY
  fi
}

for size in "${SIZES[@]}"; do
  out="$OUT_DIR/app-icon-${size}.png"
  render "$size" "$out"
  echo "✓ $out"
done

echo "Regenerated ${#SIZES[@]} icons from $(basename "$SVG")."
