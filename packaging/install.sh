#!/usr/bin/env bash
# Installs the latest Pulse release for this computer into ~/.local/bin
# (override with PULSE_PREFIX). No root needed.
#
#   curl -fsSL <this file> | bash
#
# Mac users get Pulse.app in ~/Applications (drag it to Applications to keep
# it in the menu bar every day). Windows users should use the winget or scoop
# package instead.
set -eu

REPO="${PULSE_REPO:-shreeyuvan2020/pulse}"
PREFIX="${PULSE_PREFIX:-$HOME/.local}"
OS="$(uname -s)"
ARCH="$(uname -m)"

fail() { echo "pulse-install: $*" >&2; exit 1; }

case "$OS/$ARCH" in
  Linux/x86_64)
    ASSET_PATTERN='pulse-linux-amd64.tar.gz'
    ;;
  Darwin/*)
    ASSET_PATTERN='pulse-macos-universal.tar.gz'
    ;;
  *)
    fail "no Pulse package for $OS/$ARCH yet (Windows: use winget or scoop)"
    ;;
esac

if [ -n "${PULSE_TARBALL_URL:-}" ]; then
  URL="$PULSE_TARBALL_URL"
else
  RELEASE="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest")" || fail "cannot reach GitHub"
  URL="$(printf '%s' "$RELEASE" | grep -o '"browser_download_url": *"[^"]*"' | cut -d'"' -f4 | grep "$ASSET_PATTERN" | head -1)" || true
  [ -n "$URL" ] || fail "no $ASSET_PATTERN in the latest release of $REPO"
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
echo "pulse-install: downloading $URL"
curl -fsSL "$URL" -o "$TMP/pulse.tar.gz"

if [ "$OS" = "Darwin" ]; then
  APPS="${PULSE_APPS_DIR:-$HOME/Applications}"
  mkdir -p "$APPS"
  tar xzf "$TMP/pulse.tar.gz" -C "$APPS"
  echo "pulse-install: installed $APPS/Pulse.app"
  echo "Open it once to see Pulse in your menu bar, and move it to /Applications if you want it every day."
  exit 0
fi

mkdir -p "$PREFIX/bin"
tar xzf "$TMP/pulse.tar.gz" -C "$PREFIX/bin"
chmod +x "$PREFIX/bin/pulse"
echo "pulse-install: installed $PREFIX/bin/pulse"
case ":$PATH:" in
  *":$PREFIX/bin:"*) ;;
  *) echo "Add $PREFIX/bin to your PATH, then try: pulse login <server>, pulse daemon, pulse tray" ;;
esac
"$PREFIX/bin/pulse" --version
