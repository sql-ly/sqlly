#!/usr/bin/env bash
# =============================================================================
# Fetch DuckDB's official prebuilt Windows library for one arch and point the
# CURRENT JOB at it.
#
#   bash client/apps/gpui/scripts/windows-duckdb.sh amd64|arm64
#
# On Windows, sqlly-engines links duckdb.dll instead of compiling DuckDB from
# source (see crates/sqlly-engines/Cargo.toml). Both the official release
# pipeline (Azure DevOps) and the standalone release workflow on GitHub run
# this once per leg, before that leg's first cargo command. Nothing
# DuckDB-related is checked in: the job downloads the release archive from
# GitHub and the only thing pinned here is its SHA-256.
#
# Sets, for the rest of the job (Azure logging commands, or $GITHUB_ENV and
# $GITHUB_PATH under GitHub Actions):
#   DUCKDB_LIB_DIR / DUCKDB_INCLUDE_DIR  read by libduckdb-sys's build script
#                                        (duckdb.lib + headers)
#   DUCKDB_DLL                           the DLL that ships beside SQLLY.exe
#                                        and sqlly-cli.exe
#   PATH (prepended)                     so `cargo test` binaries load it
#
# The DuckDB version comes from libduckdb-sys in client/Cargo.lock (duckdb-rs
# encodes it as 1.MAJOR_MINOR_PATCH.x, e.g. 1.10506.0 => DuckDB 1.5.6). A
# duckdb crate bump fails here until the new archives' hashes are added below:
#   curl -sL https://github.com/duckdb/duckdb/releases/download/v<ver>/duckdb-shared-libs-windows-<arch>.tar.gz | sha256sum
# (DuckDB 2.0 replaced the old libduckdb-windows-<arch>.zip with this tarball;
# the release page also lists each asset's sha256 digest.)
# =============================================================================
set -euo pipefail

arch="${1:?usage: windows-duckdb.sh amd64|arm64}"
case "$arch" in
  amd64|arm64) ;;
  *) echo "::error::unknown arch '$arch' (expected amd64 or arm64)"; exit 1 ;;
esac

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)"
crate_version="$(awk '/^name = "libduckdb-sys"$/ { getline; gsub(/version = |"/, ""); print; exit }' "$repo_root/client/Cargo.lock")"
if [ -z "$crate_version" ]; then
  echo "::error::libduckdb-sys not found in client/Cargo.lock"
  exit 1
fi
encoded="$(printf '%s' "$crate_version" | cut -d. -f2)"
version="$((encoded / 10000)).$(((encoded / 100) % 100)).$((encoded % 100))"

case "$version/$arch" in
  2.0.0/amd64) sha256=2b83169128239de5f45fe21de80f99bf436e53aa4bb24020dd951a1ba8d4b95f ;;
  2.0.0/arm64) sha256=159ce60eb587112b8c5f290b8beea56af5f13be4d13679de127ad9bfaafb7447 ;;
  *)
    echo "::error::No pinned SHA-256 for duckdb-shared-libs-windows-$arch.tar.gz v$version (libduckdb-sys $crate_version). Add it to client/apps/gpui/scripts/windows-duckdb.sh."
    exit 1
    ;;
esac

archive="duckdb-shared-libs-windows-$arch.tar.gz"
url="https://github.com/duckdb/duckdb/releases/download/v$version/$archive"
temp_root="${RUNNER_TEMP:-${AGENT_TEMPDIRECTORY:-}}"
if [ -z "$temp_root" ]; then
  echo "::error::neither RUNNER_TEMP (GitHub Actions) nor AGENT_TEMPDIRECTORY (Azure DevOps) is set"
  exit 1
fi
dest="$(cygpath -u "$temp_root")/duckdb/$version/$arch"
rm -rf "$dest"
mkdir -p "$dest"

echo "Downloading $url"
# GitHub's release-asset service answers 504 in bursts that can outlast
# curl's own short retry (2026-11-09: v1.31.0's Windows stage failed twice
# on it while the asset was fine a minute later), so retry for several
# minutes with a growing delay before failing the build.
attempt=1
until curl -fsSL --retry 3 --retry-delay 5 --retry-all-errors -o "$dest/$archive" "$url"; do
  if [ "$attempt" -ge 8 ]; then
    echo "::error::could not download $archive after $attempt attempts"
    exit 1
  fi
  wait_s=$((attempt * 15))
  echo "Download attempt $attempt failed; retrying in ${wait_s}s"
  sleep "$wait_s"
  attempt=$((attempt + 1))
done
actual="$(sha256sum "$dest/$archive" | cut -d' ' -f1)"
if [ "$actual" != "$sha256" ]; then
  echo "::error::$archive SHA-256 mismatch: got $actual, expected $sha256"
  exit 1
fi

tar -xzf "$dest/$archive" -C "$dest"
for f in duckdb.dll duckdb.lib duckdb.h; do
  if [ ! -f "$dest/$f" ]; then
    echo "::error::$archive did not contain $f"
    exit 1
  fi
done

win_dest="$(cygpath -w "$dest")"
if [ -n "${GITHUB_ENV:-}" ]; then
  {
    echo "DUCKDB_LIB_DIR=$win_dest"
    echo "DUCKDB_INCLUDE_DIR=$win_dest"
    echo "DUCKDB_DLL=$win_dest\\duckdb.dll"
  } >> "$GITHUB_ENV"
  echo "$win_dest" >> "$GITHUB_PATH"
else
  echo "##vso[task.setvariable variable=DUCKDB_LIB_DIR]$win_dest"
  echo "##vso[task.setvariable variable=DUCKDB_INCLUDE_DIR]$win_dest"
  echo "##vso[task.setvariable variable=DUCKDB_DLL]$win_dest\\duckdb.dll"
  echo "##vso[task.prependpath]$win_dest"
fi
echo "DuckDB $version ($arch) ready in $win_dest"
