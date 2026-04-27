#!/usr/bin/env bash
# Install enzyme — local-first knowledge indexing for Obsidian vaults
# Usage: curl -fsSL enzyme.garden/install.sh | bash

set -euo pipefail

REPO="jshph/enzyme"
INSTALL_DIR="$HOME/.local/bin"

# Detect platform
case "$(uname -s)-$(uname -m)" in
    Darwin-arm64)               TARGET="macos-arm64" ;;
    Darwin-x86_64)
        echo "Intel Macs are not supported." >&2
        exit 1 ;;
    Linux-x86_64)               TARGET="linux-x86_64" ;;
    Linux-aarch64)              TARGET="linux-arm64" ;;
    *)
        echo "Unsupported platform." >&2
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

# Install bundled libraries (Linux builds include lib/ with libstdc++)
if [ -d "$tmpdir/lib" ]; then
    mkdir -p "$INSTALL_DIR/lib"
    cp -f "$tmpdir/lib/"* "$INSTALL_DIR/lib/"
fi

# Clear previous data (models, indices) but preserve auth
if [ -f "$HOME/.enzyme/auth.json" ]; then
    cp "$HOME/.enzyme/auth.json" "$tmpdir/auth.json.bak"
fi
rm -rf "$HOME/.enzyme"
if [ -f "$tmpdir/auth.json.bak" ]; then
    mkdir -p "$HOME/.enzyme"
    mv "$tmpdir/auth.json.bak" "$HOME/.enzyme/auth.json"
fi

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

# Track install (non-blocking, best-effort)
curl -sfSo /dev/null -X POST https://enzyme-server-production.up.railway.app/telemetry/plugin-install \
  -H "Content-Type: application/json" \
  -d "{\"platform\":\"${TARGET}\",\"version\":\"${VERSION}\"}" 2>/dev/null &

echo "Installed to ${INSTALL_DIR}/enzyme"

# Check PATH
case ":$PATH:" in
    *":${INSTALL_DIR}:"*) ;;
    *) echo "Add to PATH: export PATH=\"${INSTALL_DIR}:\$PATH\"" ;;
esac

# Install Claude Code plugin if claude is available
if command -v claude &>/dev/null; then
    echo ""
    echo "Installing Claude Code plugin..."
    claude plugin marketplace add jshph/enzyme 2>/dev/null || true
    claude plugin install enzyme 2>/dev/null || true
fi

# Prompt login if not already authenticated
if [ ! -f "$HOME/.enzyme/auth.json" ]; then
    echo ""
    echo "Creating your Enzyme account..."
    "${INSTALL_DIR}/enzyme" login || true
fi

echo ""
echo "Next: cd into your content folder and run enzyme init."
echo ""
echo "  cd /path/to/your/vault"
echo "  enzyme init"
echo ""
echo "Enzyme will scan your vault and show you what it finds before proceeding."
if ! command -v claude &>/dev/null; then
    echo ""
    echo "Add the Claude Code plugin:"
    echo "  claude plugin marketplace add jshph/enzyme"
    echo "  claude plugin install enzyme"
fi
