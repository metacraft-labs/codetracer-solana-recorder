#!/usr/bin/env bash
#
# Build a .deb package for a CodeTracer recorder.
#
# Inputs:
#   $1 = path to the recorder's `packaging/deb/` dir (contains the
#        generated control file)
#   $2 = path to the pre-built recorder binary
#   $3 = output dir (.deb is dropped here)
#   $4 = recorder name      (matches Package: in control)
#   $5 = recorder version   (matches Version: in control)
#   $6 = binary install name (the name under /usr/bin)
#   $7 = arch (amd64 / arm64)
#
# Output:
#   $3/<recorder>_<version>_<arch>.deb
#
# Required tools on PATH: dpkg-deb. The test harness materializes
# dpkg-deb via `nix-shell -p dpkg --run "..."`.

set -euo pipefail

if [[ $# -ne 7 ]]; then
  cat >&2 <<EOF
usage: $0 <deb-dir> <binary> <out-dir> <recorder> <version> <bin-name> <arch>
EOF
  exit 1
fi

DEB_DIR="$1"
BINARY="$2"
OUT_DIR="$3"
RECORDER="$4"
VERSION="$5"
BIN_NAME="$6"
ARCH="$7"

if [[ ! -f "$DEB_DIR/control" ]]; then
  echo "::error::expected control file at $DEB_DIR/control" >&2
  exit 1
fi
if [[ ! -f "$BINARY" ]]; then
  echo "::error::expected pre-built binary at $BINARY" >&2
  exit 1
fi

mkdir -p "$OUT_DIR"

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

# Standard Debian-package layout.
mkdir -p "$STAGE/DEBIAN" "$STAGE/usr/bin"

# Per-recorder control file (already populated by generate.py).
cp "$DEB_DIR/control" "$STAGE/DEBIAN/control"

# Recorder binary at /usr/bin/<bin-name>.
install -m 755 "$BINARY" "$STAGE/usr/bin/$BIN_NAME"

# Stage license / readme / changelog if present (so dpkg-deb -I
# produces useful provenance info).
if [[ -f "$DEB_DIR/copyright" ]]; then
  mkdir -p "$STAGE/usr/share/doc/$RECORDER"
  install -m 644 "$DEB_DIR/copyright" \
    "$STAGE/usr/share/doc/$RECORDER/copyright"
fi

OUT_FILE="$OUT_DIR/${RECORDER}_${VERSION}_${ARCH}.deb"
dpkg-deb --build --root-owner-group "$STAGE" "$OUT_FILE"

echo "wrote $OUT_FILE"
echo "$OUT_FILE"
