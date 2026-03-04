#!/usr/bin/env bash
# Install enzyme — local-first knowledge indexing for Obsidian vaults
# Usage: curl -fsSL https://raw.githubusercontent.com/jshph/enzyme/main/install.sh | bash

set -euo pipefail

REPO="jshph/enzyme"
INSTALL_DIR="$HOME/.local/bin"

# Detect platform
case "$(uname -s)-$(uname -m)" in
    Darwin-arm64)               TARGET="aarch64-apple-darwin" ;;
    Darwin-x86_64)
        echo "Intel Macs are not supported. Install via Cargo: cargo install --git https://github.com/jshph/enzyme-rust --bin enzyme" >&2
        exit 1 ;;
    Linux-x86_64)               TARGET="x86_64-unknown-linux-gnu" ;;
    Linux-aarch64)              TARGET="aarch64-unknown-linux-gnu" ;;
    *)
        echo "Unsupported platform. Install via Cargo: cargo install --git https://github.com/jshph/enzyme-rust --bin enzyme" >&2
        exit 1 ;;
esac

# Fetch latest version
VERSION="$(curl -fsSL -o /dev/null -w '%{url_effective}' "https://github.com/${REPO}/releases/latest")"
VERSION="${VERSION##*/}"

echo "Installing enzyme ${VERSION} (${TARGET})..."

# Download and extract
tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT
curl -fsSL "https://github.com/${REPO}/releases/download/${VERSION}/enzyme-${TARGET}.tar.gz" | tar xz -C "$tmpdir"

# Install
mkdir -p "$INSTALL_DIR"
mv "$tmpdir/enzyme" "$INSTALL_DIR/enzyme"
chmod +x "$INSTALL_DIR/enzyme"

# Clear previous data (models, indices) for clean slate
rm -rf "$HOME/.enzyme"

echo "Installed to ${INSTALL_DIR}/enzyme"

# Check PATH
case ":$PATH:" in
    *":${INSTALL_DIR}:"*) ;;
    *) echo "Add to PATH: export PATH=\"${INSTALL_DIR}:\$PATH\"" ;;
esac

echo ""
echo "Next steps:"
echo "  enzyme setup                        # download embedding model (~52 MB)"
echo "  cd /path/to/vault && enzyme init    # initialize your vault"
echo ""
echo "Add the Claude Code plugin:"
echo "  claude plugin marketplace add jshph/enzyme"
echo "  claude plugin install enzyme"
