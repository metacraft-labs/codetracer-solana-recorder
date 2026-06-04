#!/usr/bin/env bash
#
# Build an .rpm package for a CodeTracer recorder.
#
# Inputs:
#   $1 = path to the recorder's `packaging/rpm/` dir (contains the
#        generated spec file)
#   $2 = path to the pre-built recorder binary
#   $3 = output dir (.rpm is dropped here)
#   $4 = recorder name
#   $5 = recorder version
#   $6 = binary install name
#   $7 = arch (x86_64 / aarch64)
#
# Output:
#   $3/<recorder>-<version>-1.<arch>.rpm
#
# Required tools on PATH: rpmbuild. The test harness materializes
# rpmbuild via `nix-shell -p rpm --run "..."`.

set -euo pipefail

if [[ $# -ne 7 ]]; then
  cat >&2 <<EOF
usage: $0 <rpm-dir> <binary> <out-dir> <recorder> <version> <bin-name> <arch>
EOF
  exit 1
fi

RPM_DIR="$1"
BINARY="$2"
OUT_DIR="$3"
RECORDER="$4"
VERSION="$5"
BIN_NAME="$6"
ARCH="$7"

SPEC_FILE="$RPM_DIR/$RECORDER.spec"
if [[ ! -f "$SPEC_FILE" ]]; then
  echo "::error::expected spec file at $SPEC_FILE" >&2
  exit 1
fi
if [[ ! -f "$BINARY" ]]; then
  echo "::error::expected pre-built binary at $BINARY" >&2
  exit 1
fi

mkdir -p "$OUT_DIR"
TOPDIR="$(mktemp -d)"
trap 'rm -rf "$TOPDIR"' EXIT

mkdir -p "$TOPDIR"/{BUILD,RPMS,SOURCES,SPECS,SRPMS,BUILDROOT}
cp "$SPEC_FILE" "$TOPDIR/SPECS/$RECORDER.spec"

# Run rpmbuild without network access; the spec injects the pre-built
# binary via --define "_binary_source <abs-path>" so we don't need
# Source0.
rpmbuild \
  --define "_topdir $TOPDIR" \
  --define "_tmppath $TOPDIR/tmp" \
  --define "_binary_source $BINARY" \
  --define "_build_id_links none" \
  --define "_binary_payload w2.xzdio" \
  --target "$ARCH" \
  -bb "$TOPDIR/SPECS/$RECORDER.spec"

OUT_FILE="$TOPDIR/RPMS/$ARCH/${RECORDER}-${VERSION}-1.${ARCH}.rpm"
if [[ ! -f "$OUT_FILE" ]]; then
  # Fall back: some rpm builds tack on a .dist suffix; find whatever
  # rpmbuild actually produced.
  OUT_FILE="$(find "$TOPDIR/RPMS" -name "${RECORDER}-${VERSION}-*.${ARCH}.rpm" \
    -type f | head -n1)"
fi
if [[ -z "$OUT_FILE" || ! -f "$OUT_FILE" ]]; then
  echo "::error::rpmbuild claimed success but no .rpm in $TOPDIR/RPMS" >&2
  ls -la "$TOPDIR/RPMS" >&2
  exit 1
fi

DEST="$OUT_DIR/$(basename "$OUT_FILE")"
cp "$OUT_FILE" "$DEST"
echo "wrote $DEST"
echo "$DEST"
