#!/usr/bin/env bash
#
# Build a Chocolatey .nupkg for a CodeTracer recorder.
#
# Chocolatey packages are NuGet zip archives (.nupkg) with a
# .nuspec metadata file and tools/ scripts inside. Submission flow:
#
#   choco push <recorder>.<version>.nupkg \
#     --source https://push.chocolatey.org/ \
#     --api-key $CHOCOLATEY_API_KEY
#
# This script:
#   1. Stages the recorder's already-rendered nuspec + tools/ tree
#      into a fresh build dir.
#   2. Optionally rewrites the SHA placeholder in
#      tools/chocolateyInstall.ps1 from a supplied zip artifact.
#   3. Calls `choco pack` (or `nuget pack` as a fallback) to produce
#      the .nupkg. On Linux/macOS hosts without `choco`, it falls
#      back to creating a NuPkg by hand via `zip`.
#
# Inputs:
#   $1 = path to the recorder's `packaging/chocolatey/` dir
#        (contains the rendered <recorder>.nuspec + tools/)
#   $2 = path to the pre-built windows-x86_64 zip (used for sha256)
#        — pass /dev/null or "-" to leave the placeholder in place
#   $3 = output dir (.nupkg dropped here)
#   $4 = recorder name (must match the nuspec <id>)
#   $5 = recorder version (must match the nuspec <version>)
#
# Output:
#   $3/<recorder>.<version>.nupkg
#
# Required tools on PATH: zip + sha256sum. Both available via
# `nix-shell -p zip coreutils`.

set -euo pipefail

if [[ $# -ne 5 ]]; then
  cat >&2 <<EOF
usage: $0 <chocolatey-dir> <zip-artifact|-> <out-dir> <recorder> <version>
EOF
  exit 1
fi

CHOCO_DIR="$1"
ZIP_PATH="$2"
OUT_DIR="$3"
RECORDER="$4"
VERSION="$5"

NUSPEC_IN="$CHOCO_DIR/$RECORDER.nuspec"
INSTALL_IN="$CHOCO_DIR/tools/chocolateyInstall.ps1"
UNINSTALL_IN="$CHOCO_DIR/tools/chocolateyUninstall.ps1"

for f in "$NUSPEC_IN" "$INSTALL_IN" "$UNINSTALL_IN"; do
  if [[ ! -f "$f" ]]; then
    echo "::error::expected file $f" >&2
    exit 1
  fi
done

mkdir -p "$OUT_DIR"

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

mkdir -p "$STAGE/tools"
cp "$NUSPEC_IN"   "$STAGE/$RECORDER.nuspec"
cp "$INSTALL_IN"  "$STAGE/tools/chocolateyInstall.ps1"
cp "$UNINSTALL_IN" "$STAGE/tools/chocolateyUninstall.ps1"

# Substitute the SHA placeholder if a real zip was supplied.
if [[ "$ZIP_PATH" != "-" && "$ZIP_PATH" != "/dev/null" ]]; then
  if [[ ! -f "$ZIP_PATH" ]]; then
    echo "::error::expected windows-x86_64 zip at $ZIP_PATH" >&2
    exit 1
  fi
  SHA=$(sha256sum "$ZIP_PATH" | awk '{print $1}')
  sed -i "s/REPLACE_AT_PUBLISH_TIME_windows_x86_64/$SHA/" \
    "$STAGE/tools/chocolateyInstall.ps1"
fi

# Prefer real `choco pack`; fall back to a hand-rolled .nupkg via
# `zip` so this script runs unchanged on Linux/macOS CI hosts.
OUT_FILE="$OUT_DIR/${RECORDER}.${VERSION}.nupkg"

if command -v choco >/dev/null 2>&1; then
  ( cd "$STAGE" && choco pack "$RECORDER.nuspec" --out "$OUT_DIR" )
elif command -v nuget >/dev/null 2>&1; then
  ( cd "$STAGE" && nuget pack "$RECORDER.nuspec" -OutputDirectory "$OUT_DIR" )
else
  # Hand-rolled .nupkg: NuGet's .nupkg format is a plain zip with the
  # nuspec at the archive root and tools/ alongside. Add the standard
  # [Content_Types].xml + _rels/.rels stubs so the file is parseable
  # by nuget tooling on the consumer side.
  cat > "$STAGE/[Content_Types].xml" <<EOF
<?xml version="1.0" encoding="utf-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="nuspec" ContentType="application/octet" />
  <Default Extension="ps1"    ContentType="application/octet" />
  <Default Extension="rels"   ContentType="application/vnd.openxmlformats-package.relationships+xml" />
</Types>
EOF
  mkdir -p "$STAGE/_rels"
  cat > "$STAGE/_rels/.rels" <<EOF
<?xml version="1.0" encoding="utf-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Type="http://schemas.microsoft.com/packaging/2010/07/manifest"
                Target="/${RECORDER}.nuspec" Id="R0" />
</Relationships>
EOF
  ( cd "$STAGE" && zip -qr "$OUT_FILE" . )
fi

if [[ ! -f "$OUT_FILE" ]]; then
  echo "::error::no .nupkg produced at $OUT_FILE" >&2
  ls -la "$OUT_DIR" >&2
  exit 1
fi

echo "wrote $OUT_FILE"
echo "$OUT_FILE"
