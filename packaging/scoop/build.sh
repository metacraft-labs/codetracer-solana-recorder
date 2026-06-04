#!/usr/bin/env bash
#
# Build a Scoop manifest JSON for a CodeTracer recorder.
#
# Scoop manifests live in a "bucket" repository (a git repo of
# JSON files). Users do:
#
#   scoop bucket add codetracer https://github.com/metacraft-labs/scoop-codetracer
#   scoop install <recorder>
#
# This script:
#   1. Accepts a pre-built windows-x86_64 zip artifact path.
#   2. Computes its sha256.
#   3. Substitutes {{SCOOP_X86_64_HASH}} into the on-disk manifest
#      (already rendered by generate.py with the SHA placeholder).
#   4. Writes the final <recorder>.json into the output dir.
#
# Inputs:
#   $1 = path to the recorder's `packaging/scoop/` dir (contains the
#        generated <recorder>.json manifest with the SHA placeholder)
#   $2 = path to the pre-built windows-x86_64 zip
#   $3 = output dir (final <recorder>.json is dropped here)
#   $4 = recorder name (matches the JSON file name)
#
# Output:
#   $3/<recorder>.json   (manifest with SHA filled in)
#
# Required tools on PATH: sha256sum. Universally available on
# Linux/macOS hosts (and inside `nix-shell -p coreutils`).

set -euo pipefail

if [[ $# -ne 4 ]]; then
  cat >&2 <<EOF
usage: $0 <scoop-dir> <zip-artifact> <out-dir> <recorder>
EOF
  exit 1
fi

SCOOP_DIR="$1"
ZIP_PATH="$2"
OUT_DIR="$3"
RECORDER="$4"

MANIFEST_IN="$SCOOP_DIR/$RECORDER.json"
if [[ ! -f "$MANIFEST_IN" ]]; then
  echo "::error::expected manifest at $MANIFEST_IN" >&2
  exit 1
fi
if [[ ! -f "$ZIP_PATH" ]]; then
  echo "::error::expected windows-x86_64 zip at $ZIP_PATH" >&2
  exit 1
fi

mkdir -p "$OUT_DIR"

SHA=$(sha256sum "$ZIP_PATH" | awk '{print $1}')
if [[ -z "$SHA" ]]; then
  echo "::error::sha256sum produced no hash for $ZIP_PATH" >&2
  exit 1
fi

OUT_FILE="$OUT_DIR/$RECORDER.json"
# Substitute the SHA placeholder. The placeholder is the literal
# string `REPLACE_AT_PUBLISH_TIME_windows_x86_64` (matches the
# Homebrew formula convention) when the manifest has not been
# through the scoop build step before; on subsequent runs the
# already-substituted manifest contains a hex SHA, which we leave
# in place if --keep-existing-hash is implied (the publish workflow
# always computes a fresh hash).
sed "s/REPLACE_AT_PUBLISH_TIME_windows_x86_64/$SHA/" \
  "$MANIFEST_IN" > "$OUT_FILE"

echo "wrote $OUT_FILE (sha256=$SHA)"
echo "$OUT_FILE"
