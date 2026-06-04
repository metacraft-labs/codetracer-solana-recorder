#!/usr/bin/env bash
#
# Build all five OS-packaging channels for codetracer-solana-recorder.
#
# Output:
#   packaging/dist/<channel>/<artifact>
#
# Channels:
#   homebrew  → no local build; just a sanity check (formula validity)
#   nix       → nix-build packaging/nix/default.nix --arg srcOverride ./.
#   aur       → no local build; PKGBUILD is consumed by makepkg on Arch
#   deb       → packaging/deb/build.sh -> .deb
#   rpm       → packaging/rpm/build.sh -> .rpm
#
# Required env (optional):
#   CT_RECORDER_BINARY  Pre-built binary path. If unset, the script
#                       runs `cargo build --release` to produce one.
#   CT_PACKAGING_CHANNELS  Comma-separated list to limit which
#                       channels to build (default: all).

set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
PKG_ROOT="$REPO_ROOT/packaging"
DIST="$PKG_ROOT/dist"
mkdir -p "$DIST"

# Parse the recorder name + version straight out of the metadata so
# this script stays generic (works for any recorder when copied).
RECORDER_NAME="$(grep -E '^\s*name:' "$PKG_ROOT/recorder-metadata.yml" \
  | head -n1 | awk -F: '{print $2}' | xargs)"
VERSION="$(grep -E '^\s*version:' "$PKG_ROOT/recorder-metadata.yml" \
  | head -n1 | awk -F: '{print $2}' | xargs)"
BINARY_NAME="$(grep -E '^\s*binary-name:' "$PKG_ROOT/recorder-metadata.yml" \
  | head -n1 | awk -F: '{print $2}' | xargs)"

# Sanity-print parsed metadata so a failed run leaves a breadcrumb.
echo "[m11] recorder=$RECORDER_NAME version=$VERSION binary=$BINARY_NAME"

# Determine which channels to build.
CHANNELS="${CT_PACKAGING_CHANNELS:-homebrew,nix,aur,deb,rpm}"

# --- Locate / build the recorder binary -------------------------------

if [[ -z "${CT_RECORDER_BINARY:-}" ]]; then
  echo "[m11] CT_RECORDER_BINARY unset — running cargo build --release"
  (cd "$REPO_ROOT" && cargo build --release --locked --bin "$BINARY_NAME")
  CT_RECORDER_BINARY="$REPO_ROOT/target/release/$BINARY_NAME"
fi

if [[ ! -f "$CT_RECORDER_BINARY" ]]; then
  echo "::error::recorder binary not found at $CT_RECORDER_BINARY" >&2
  exit 1
fi

ARCH_DEB="amd64"
ARCH_RPM="x86_64"
case "$(uname -m)" in
  aarch64|arm64)
    ARCH_DEB="arm64"
    ARCH_RPM="aarch64"
    ;;
esac

# --- Per-channel build steps ------------------------------------------

build_homebrew() {
  echo "[m11] homebrew: validating formula syntax"
  if command -v ruby >/dev/null 2>&1; then
    ruby -c "$PKG_ROOT/homebrew/$RECORDER_NAME.rb"
  else
    echo "[m11] ruby not on PATH — skipping syntax check"
  fi
  mkdir -p "$DIST/homebrew"
  cp "$PKG_ROOT/homebrew/$RECORDER_NAME.rb" "$DIST/homebrew/"
}

build_nix() {
  echo "[m11] nix: nix-build packaging/nix/default.nix"
  if ! command -v nix-build >/dev/null 2>&1; then
    echo "::warning::nix-build not on PATH — skipping nix channel"
    return 0
  fi
  mkdir -p "$DIST/nix"
  (cd "$REPO_ROOT" && \
    nix-build packaging/nix/default.nix \
      --arg srcOverride ./. \
      --argstr version "$VERSION" \
      -o "$DIST/nix/result") || {
    echo "::warning::nix-build failed; the derivation may need adjustment"
    return 0
  }
}

build_aur() {
  echo "[m11] aur: copying PKGBUILD + .SRCINFO into dist"
  mkdir -p "$DIST/aur"
  cp "$PKG_ROOT/aur/PKGBUILD"  "$DIST/aur/"
  cp "$PKG_ROOT/aur/.SRCINFO"  "$DIST/aur/"
}

build_deb() {
  echo "[m11] deb: dpkg-deb --build"
  if ! command -v dpkg-deb >/dev/null 2>&1; then
    echo "::warning::dpkg-deb not on PATH — skipping deb channel"
    return 0
  fi
  mkdir -p "$DIST/deb"
  bash "$PKG_ROOT/deb/build.sh" \
    "$PKG_ROOT/deb" \
    "$CT_RECORDER_BINARY" \
    "$DIST/deb" \
    "$RECORDER_NAME" \
    "$VERSION" \
    "$BINARY_NAME" \
    "$ARCH_DEB"
}

build_rpm() {
  echo "[m11] rpm: rpmbuild -bb"
  if ! command -v rpmbuild >/dev/null 2>&1; then
    echo "::warning::rpmbuild not on PATH — skipping rpm channel"
    return 0
  fi
  mkdir -p "$DIST/rpm"
  bash "$PKG_ROOT/rpm/build.sh" \
    "$PKG_ROOT/rpm" \
    "$CT_RECORDER_BINARY" \
    "$DIST/rpm" \
    "$RECORDER_NAME" \
    "$VERSION" \
    "$BINARY_NAME" \
    "$ARCH_RPM"
}

IFS=',' read -ra CHANNEL_ARRAY <<< "$CHANNELS"
for ch in "${CHANNEL_ARRAY[@]}"; do
  case "$ch" in
    homebrew) build_homebrew ;;
    nix)      build_nix      ;;
    aur)      build_aur      ;;
    deb)      build_deb      ;;
    rpm)      build_rpm      ;;
    *) echo "::warning::unknown channel '$ch' — skipping" ;;
  esac
done

echo "[m11] done — artifacts under $DIST"
ls -R "$DIST"
