#!/usr/bin/env bash
# Install enzyme — local-first knowledge indexing for markdown vaults
# Usage: curl -fsSL https://raw.githubusercontent.com/byenzyme/enzyme/main/install.sh | bash

set -euo pipefail

REPO="byenzyme/enzyme"
INSTALL_DIR="$HOME/.local/bin"

# Detect platform
case "$(uname -s)-$(uname -m)" in
    Darwin-arm64)               TARGET="macos-arm64" ;;
    Darwin-x86_64)              TARGET="macos-x86_64" ;;
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
archive="enzyme-${TARGET}.tar.gz"
archive_url="https://github.com/${REPO}/releases/download/${VERSION}/${archive}"
curl -fsSL -o "$tmpdir/$archive" "$archive_url"

# Verify against the checksum published with the release before extracting.
expected="$(curl -fsSL "${archive_url}.sha256" | awk '{print $1}')"
if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$tmpdir/$archive" | awk '{print $1}')"
else
    actual="$(shasum -a 256 "$tmpdir/$archive" | awk '{print $1}')"
fi
if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then
    echo "Checksum verification failed for ${archive}; not installing." >&2
    exit 1
fi
tar xzf "$tmpdir/$archive" -C "$tmpdir"

# Install
mkdir -p "$INSTALL_DIR"
mv "$tmpdir/enzyme" "$INSTALL_DIR/enzyme"
chmod +x "$INSTALL_DIR/enzyme"

# Install bundled libraries (Linux builds include lib/ with libstdc++)
if [ -d "$tmpdir/lib" ]; then
    mkdir -p "$INSTALL_DIR/lib"
    cp -f "$tmpdir/lib/"* "$INSTALL_DIR/lib/"
fi

# Remove legacy generated embedding model cache. Current Enzyme uses the
# compiled-in ESE backend, so these files are stale app state, not user data.
legacy_model_dir="$HOME/.cache/enzyme/models"
if [ -d "$legacy_model_dir" ] && \
   { [ -f "$legacy_model_dir/model.safetensors" ] || \
     [ -f "$legacy_model_dir/tokenizer.json" ] || \
     [ -f "$legacy_model_dir/config.json" ]; }; then
    rm -rf "$legacy_model_dir"
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
curl -sfSo /dev/null -X POST https://api.enzyme.garden/telemetry/plugin-install \
  -H "Content-Type: application/json" \
  -d "{\"platform\":\"${TARGET}\",\"version\":\"${VERSION}\"}" 2>/dev/null &

echo "Installed to ${INSTALL_DIR}/enzyme"

# Check PATH
case ":$PATH:" in
    *":${INSTALL_DIR}:"*) ;;
    *) echo "Add to PATH: export PATH=\"${INSTALL_DIR}:\$PATH\"" ;;
esac

# First vault initialization can use Enzyme's hosted bootstrap without login.
# Login remains available for refresh, additional vaults, publishing, and account credits.
if [ ! -f "$HOME/.enzyme/auth.json" ]; then
    echo ""
    echo "No login required for your first vault init. Later, run: enzyme login"
fi

echo ""
echo "Next: install agent instructions from your notes folder."
echo ""
echo "  cd /path/to/your/notes"
echo "  enzyme install claude     # Claude Code"
echo "  enzyme install codex      # Codex / Pi / generic .agents"
echo "  enzyme install hermes     # Hermes (experimental)"
echo "  enzyme install openclaw   # OpenClaw (experimental)"
echo ""
echo "Already set up? Run it again to update the agent instructions."
echo ""
echo "Agents: read the installed enzyme-workspace-setup skill and follow it."
echo "It covers settings, review with the user, indexing, and a first answer."
echo ""
echo "Setting up by hand? Paste the setup prompt from https://byenzyme.com/setup"
echo "into your agent, or run: enzyme compile -v, review the saved settings,"
echo "then enzyme init."
