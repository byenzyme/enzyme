#!/bin/sh
# Bootstrap enzyme from the bundled skill — copies the platform binary into
# ~/.cache/enzyme, creates a symlink in ~/.local/bin.
#
# Usage: sh setup.sh [SKILL_ROOT]
#   SKILL_ROOT defaults to the parent of scripts/ (i.e. the enzyme/ skill dir).
set -e

if [ -n "$1" ]; then
    SKILL_ROOT="$1"
else
    SKILL_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
fi

# Homebrew owns its own upgrades. For every other install path, keep going so a
# newer bundled plugin binary can replace a stale cached copy.
if command -v enzyme >/dev/null 2>&1; then
    case "$(command -v enzyme)" in
        /opt/homebrew/*|/usr/local/*) exit 0 ;;
    esac
fi

# Detect platform.
OS=$(uname -s)
ARCH=$(uname -m)
case "${OS}-${ARCH}" in
    Darwin-arm64)   BIN="enzyme-macos-arm64"  ;;
    Linux-x86_64)   BIN="enzyme-linux-x86_64" ;;
    Linux-aarch64)  BIN="enzyme-linux-arm64"   ;;
    *)
        echo "enzyme: unsupported platform ${OS}-${ARCH}" >&2
        exit 0
        ;;
esac

SRC="${SKILL_ROOT}/bin/${BIN}"
if [ ! -f "$SRC" ]; then
    echo "enzyme: platform binary not found: ${SRC}" >&2
    exit 0
fi

CACHE_DIR="$HOME/.cache/enzyme"
CACHE_BIN="${CACHE_DIR}/enzyme"

# Copy binary if missing or stale.
if [ ! -x "$CACHE_BIN" ] || [ "$SRC" -nt "$CACHE_BIN" ]; then
    mkdir -p "$CACHE_DIR"
    cp "$SRC" "$CACHE_BIN"
    chmod +x "$CACHE_BIN"
fi

# Symlink into PATH.
mkdir -p "$HOME/.local/bin"
ln -sf "$CACHE_BIN" "$HOME/.local/bin/enzyme"

echo "enzyme: installed to ~/.local/bin/enzyme"
