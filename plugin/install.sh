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

# Clean up legacy enzyme-python installation
legacy=""

if [ -d "$HOME/.local/lib/enzyme" ]; then
    rm -rf "$HOME/.local/lib/enzyme"
    legacy="1"
fi

if [ -f "$HOME/.local/bin/enzyme-mcp" ]; then
    rm -f "$HOME/.local/bin/enzyme-mcp"
    legacy="1"
fi

# Remove old MCP server entries from Claude Code, Claude Desktop, Codex
if command -v claude &>/dev/null; then
    claude mcp remove enzyme -s user 2>/dev/null && legacy="1"
fi

for config in \
    "$HOME/Library/Application Support/Claude/claude_desktop_config.json" \
    "$HOME/.config/Claude/claude_desktop_config.json"; do
    if [ -f "$config" ] && grep -q '"enzyme"' "$config"; then
        python3 -c "
import json, sys
p = sys.argv[1]
with open(p) as f: c = json.load(f)
if 'mcpServers' in c and 'enzyme' in c['mcpServers']:
    del c['mcpServers']['enzyme']
    with open(p, 'w') as f: json.dump(c, f, indent=2)
        " "$config" 2>/dev/null && legacy="1"
    fi
done

codex_config="$HOME/.codex/config.toml"
if [ -f "$codex_config" ] && grep -q 'mcp_servers\.enzyme' "$codex_config"; then
    python3 -c "
import sys
lines = open(sys.argv[1]).readlines()
skip, out = False, []
for line in lines:
    if line.strip() == '[mcp_servers.enzyme]':
        skip = True
        continue
    if skip and line.strip().startswith('['):
        skip = False
    if not skip:
        out.append(line)
open(sys.argv[1], 'w').writelines(out)
    " "$codex_config" 2>/dev/null && legacy="1"
fi

[ -n "$legacy" ] && echo "Cleaned up legacy enzyme-python installation."

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
