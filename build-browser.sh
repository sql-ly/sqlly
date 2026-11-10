#!/usr/bin/env bash
# =============================================================================
# build-browser.sh — build the SQLLY browser preview (in-tab demo).
#
# This is a labeled demo, not a Cloud frontend and not for production
# servers. CI keeps wasm as a compile check (`just wasm-check`); this
# script is the optional full static-site bundle.
#
# Compiles the real gpui application stack to WebAssembly via gpui's web
# platform backend (gpui_web, zed git tree — see [patch.crates-io] in
# client/Cargo.toml) and assembles a self-contained static site in
# client/apps/browser/dist/:
#   index.html                     (from client/apps/browser/www)
#   sqlly_browser.js + sqlly_browser_bg.wasm   (wasm-bindgen output)
#   assets/icons/*.svg             (gpui-component icon set, fetched at
#                                   runtime by the wasm asset loader)
#
# Serve dist/ from any static host; the wasm module must come over HTTP,
# not file://.
#
# Requirements:
#   rustup toolchain install nightly-2026-08-13   (gpui_web's wasm_thread dependency uses
#                                       #![feature]; native builds stay on the
#                                       workspace's stable toolchain)
#   rustup target add wasm32-unknown-unknown --toolchain nightly-2026-08-13
#   cargo install wasm-bindgen-cli   (version matching Cargo.lock's
#                                     wasm-bindgen; the script checks)
#
# Override the pinned nightly with a floating one when experimenting:
#   TOOLCHAIN=+nightly ./build-browser.sh
# =============================================================================

set -euo pipefail

repo_root="$(cd "$(dirname "$0")" && pwd)"
cd "$repo_root/client"

# Nightly is scoped to this wasm leg only — the native app builds on stable.
# Pin a dated toolchain so CI and local full-browser builds stay reproducible;
# pass TOOLCHAIN=+nightly to float forward when testing a newer nightly.
TOOLCHAIN="${TOOLCHAIN:-+nightly-2026-08-13}"
TARGET=wasm32-unknown-unknown

# sqlite-wasm-rs compiles SQLite's C for wasm and needs a wasm-capable clang
# + llvm-ar. When none is configured, fall back to the zig-based shims
# (zig cc is a clang frontend); see tools/zig-wasm-cc.bat.
if ! command -v clang >/dev/null 2>&1; then
  export CC_wasm32_unknown_unknown="${CC_wasm32_unknown_unknown:-$repo_root/tools/zig-wasm-cc.bat}"
  export AR_wasm32_unknown_unknown="${AR_wasm32_unknown_unknown:-$repo_root/tools/zig-wasm-ar.bat}"
fi
# release-ci: thin LTO / 8 codegen units — near-release perf without the fat
# LTO wall-clock; the shipped native binary is unaffected (it uses --release).
PROFILE="${PROFILE:-release-ci}"

command -v wasm-bindgen >/dev/null 2>&1 || {
  echo "ERROR: wasm-bindgen not found — cargo install wasm-bindgen-cli" >&2
  exit 1
}
rustup target list --installed --toolchain "${TOOLCHAIN#+}" | grep -q "$TARGET" \
  || rustup target add "$TARGET" --toolchain "${TOOLCHAIN#+}"

# The CLI and the crate's wasm-bindgen must match: a mismatch produces broken
# glue JS at runtime, not a build error.
crate_bindgen="$(cargo tree -p sqlly-browser --target "$TARGET" -i wasm-bindgen --depth 0 2>/dev/null | head -1 | sed 's/.* v//;s/ .*//')" || true
cli_bindgen="$(wasm-bindgen --version | awk '{print $2}')"
if [ -n "${crate_bindgen:-}" ] && [ "$crate_bindgen" != "$cli_bindgen" ]; then
  echo "ERROR: wasm-bindgen version mismatch: crate $crate_bindgen vs CLI $cli_bindgen." >&2
  echo "       cargo install wasm-bindgen-cli --version $crate_bindgen --force" >&2
  exit 1
fi

echo "==> cargo $TOOLCHAIN build -p sqlly-browser --target $TARGET --profile $PROFILE"
cargo "$TOOLCHAIN" build -p sqlly-browser --target "$TARGET" --profile "$PROFILE"

# CI points CARGO_TARGET_DIR at a persistent tree outside the checkout (the
# release pipeline's wasm job) so `checkout: clean` can't wipe it; honor it
# here or the build succeeds and then the artifact lookup fails.
wasm_path="${CARGO_TARGET_DIR:-target}/$TARGET/$PROFILE/sqlly_browser.wasm"
[ -f "$wasm_path" ] || { echo "ERROR: $wasm_path not found" >&2; exit 1; }

dist="apps/browser/dist"
# Clear contents rather than removing the directory: a dev server with its
# cwd inside dist keeps the dir handle open on Windows, and `rm -rf dist`
# would then delete the files, fail on the dir, and abort the script —
# leaving an empty site.
mkdir -p "$dist"
rm -rf "$dist"/* 2>/dev/null || true

echo "==> wasm-bindgen --target web"
wasm-bindgen "$wasm_path" --target web --out-dir "$dist" --no-typescript

cp apps/browser/www/index.html "$dist/"
# boot.js is an external module (kept out of index.html) so the page loads
# under the marketing site's strict CSP without a per-path exemption.
cp apps/browser/www/boot.js "$dist/"
# The loading screen shows the app logo before any wasm is available.
cp "$repo_root/assets/sqlly2.svg" "$dist/"

# NOTE: gpui-component's icon SVGs are embedded directly in the wasm binary
# (apps/browser embeds apps/browser/assets/icons via include_dir), so there is
# no longer an icon set to stage into dist/ — the app serves them synchronously
# from memory rather than fetching over HTTP.

# Northwind sample database (jpwhite3/northwind-SQLite3). Large (~24 MB) and
# never committed (see .gitignore); cached under www/ and downloaded only when
# missing, then staged into dist/ where the app fetches it at runtime. A
# download failure is non-fatal: the browser app falls back to the emulated
# demo database when the file is absent.
northwind_url="https://github.com/jpwhite3/northwind-SQLite3/raw/main/dist/northwind.db"
northwind_cache="apps/browser/www/northwind.db"
if [ ! -f "$northwind_cache" ]; then
  echo "==> downloading Northwind sample database (~24 MB, one time)"
  if ! curl -fSL --retry 3 -o "$northwind_cache" "$northwind_url"; then
    echo "WARNING: could not download the Northwind sample database from" >&2
    echo "         $northwind_url" >&2
    echo "         The browser app will fall back to the emulated demo database." >&2
    rm -f "$northwind_cache"
  fi
else
  echo "==> using cached Northwind sample database ($northwind_cache)"
fi
if [ -f "$northwind_cache" ]; then
  cp "$northwind_cache" "$dist/northwind.db"
fi

# PGlite (Postgres compiled to wasm, @electric-sql/pglite) and the Chinook
# Postgres sample it loads at boot. Both are cached under www/ and never
# committed (see .gitignore). Failures are non-fatal: the host page logs a
# warning and the Chinook sample connection reports the engine unavailable,
# while the SQLite sample keeps working.
pglite_version="0.5.8"
pglite_url="https://registry.npmjs.org/@electric-sql/pglite/-/pglite-$pglite_version.tgz"
pglite_cache="apps/browser/www/pglite"
if [ ! -d "$pglite_cache" ]; then
  echo "==> downloading PGlite $pglite_version (~8 MB, one time)"
  pglite_tgz="$(mktemp)"
  if curl -fSL --retry 3 -o "$pglite_tgz" "$pglite_url"; then
    mkdir -p "$pglite_cache"
    tar -xzf "$pglite_tgz" -C "$pglite_cache" --strip-components=2 package/dist
    # The browser sample only needs the ESM runtime (index.js + chunks),
    # pglite.wasm/.data, and the fs/live/worker modules index.js may pull in.
    # Extension bundles and CJS/type-definition duplicates are dead weight.
    rm -rf "$pglite_cache/contrib"
    find "$pglite_cache" \( -name '*.tar.gz' -o -name '*.cjs' -o -name '*.d.cts' -o -name '*.map' \) -delete
  else
    echo "WARNING: could not download PGlite from $pglite_url" >&2
    echo "         The Chinook (PGlite) sample connection will be unavailable." >&2
    rm -rf "$pglite_cache"
  fi
  rm -f "$pglite_tgz"
else
  echo "==> using cached PGlite ($pglite_cache)"
fi
if [ -d "$pglite_cache" ]; then
  mkdir -p "$dist/pglite"
  cp -R "$pglite_cache"/. "$dist/pglite/"
fi

chinook_url="https://github.com/lerocha/chinook-database/releases/download/v1.4.5/Chinook_PostgreSql.sql"
chinook_cache="apps/browser/www/chinook_postgres.sql"
if [ ! -f "$chinook_cache" ]; then
  echo "==> downloading Chinook Postgres sample (~0.6 MB, one time)"
  if ! curl -fSL --retry 3 -o "$chinook_cache" "$chinook_url"; then
    echo "WARNING: could not download the Chinook sample from $chinook_url" >&2
    echo "         The Chinook (PGlite) sample connection will be unavailable." >&2
    rm -f "$chinook_cache"
  fi
else
  echo "==> using cached Chinook Postgres sample ($chinook_cache)"
fi
if [ -f "$chinook_cache" ]; then
  cp "$chinook_cache" "$dist/chinook_postgres.sql"
fi

echo "==> done: client/$dist"
ls -la "$dist"
