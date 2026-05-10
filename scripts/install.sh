#!/usr/bin/env sh
# Install the latest fragua release.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/kidandcat/fragua/master/scripts/install.sh | sh
#
# Env vars:
#   FRAGUA_VERSION  Specific tag to install (default: latest)
#   FRAGUA_PREFIX   Install prefix (default: /usr/local, falls back to ~/.local)

set -eu

REPO="kidandcat/fragua"
VERSION="${FRAGUA_VERSION:-latest}"
PREFIX="${FRAGUA_PREFIX:-}"

err() { printf 'fragua-install: %s\n' "$*" >&2; exit 1; }
log() { printf 'fragua-install: %s\n' "$*"; }

need() { command -v "$1" >/dev/null 2>&1 || err "missing dependency: $1"; }

need uname
need tar
if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL "$1" -o "$2"; }
  fetch_stdout() { curl -fsSL "$1"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -qO "$2" "$1"; }
  fetch_stdout() { wget -qO- "$1"; }
else
  err "need curl or wget"
fi

os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
  Darwin)
    case "$arch" in
      arm64|aarch64) label="macos-arm64" ;;
      x86_64)        label="macos-x64" ;;
      *) err "unsupported macOS arch: $arch" ;;
    esac
    ;;
  Linux)
    case "$arch" in
      x86_64) label="linux-x64" ;;
      *) err "unsupported Linux arch: $arch (only x86_64 today)" ;;
    esac
    ;;
  MINGW*|MSYS*|CYGWIN*)
    err "Windows: download fragua-<ver>-windows-x64.zip from https://github.com/$REPO/releases"
    ;;
  *) err "unsupported OS: $os" ;;
esac

if [ "$VERSION" = "latest" ]; then
  log "resolving latest release..."
  tag="$(fetch_stdout "https://api.github.com/repos/$REPO/releases/latest" \
    | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n1)"
  [ -n "$tag" ] || err "could not resolve latest tag"
else
  tag="$VERSION"
fi
log "version: $tag"
log "target:  $label"

asset="fragua-${tag}-${label}.tar.gz"
url="https://github.com/$REPO/releases/download/${tag}/${asset}"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

log "downloading $asset..."
fetch "$url" "$tmp/$asset" || err "download failed: $url"

if fetch "$url.sha256" "$tmp/$asset.sha256" 2>/dev/null; then
  log "verifying checksum..."
  ( cd "$tmp" && shasum -a 256 -c "$asset.sha256" >/dev/null ) \
    || err "checksum mismatch"
fi

log "extracting..."
( cd "$tmp" && tar -xzf "$asset" )
extracted_dir="$tmp/fragua-${tag}-${label}"
[ -x "$extracted_dir/fragua" ] || err "binary missing after extract"

# Pick install prefix.
if [ -z "$PREFIX" ]; then
  if [ -w /usr/local/bin ] 2>/dev/null; then
    PREFIX="/usr/local"
  elif command -v sudo >/dev/null 2>&1 && [ -d /usr/local/bin ]; then
    PREFIX="/usr/local"
    SUDO="sudo"
  else
    PREFIX="$HOME/.local"
    mkdir -p "$PREFIX/bin"
  fi
fi
SUDO="${SUDO:-}"

dest="$PREFIX/bin/fragua"
log "installing to $dest"
$SUDO install -m 0755 "$extracted_dir/fragua" "$dest"

# macOS: clear quarantine so Gatekeeper doesn't block the unsigned binary.
if [ "$os" = "Darwin" ] && command -v xattr >/dev/null 2>&1; then
  $SUDO xattr -d com.apple.quarantine "$dest" 2>/dev/null || true
fi

# Linux runtime deps reminder.
if [ "$os" = "Linux" ]; then
  if ! ldconfig -p 2>/dev/null | grep -q libwebkit2gtk-4.1; then
    log "warning: libwebkit2gtk-4.1 not found. Install it, e.g.:"
    log "  sudo apt install libwebkit2gtk-4.1-0 libayatana-appindicator3-1"
  fi
fi

# PATH hint if installing to ~/.local.
case ":$PATH:" in
  *":$PREFIX/bin:"*) ;;
  *) log "note: $PREFIX/bin is not on \$PATH. Add it to your shell profile." ;;
esac

log "done. Run: fragua"
