#!/bin/sh
# Set up enzyme for environments where the plugin directory may be read-only
# (e.g., Claude Code Cowork VMs). Called by the SessionStart hook via:
#   sh ${CLAUDE_PLUGIN_ROOT}/bin/setup.sh
#
# Creates:
#   ~/.cache/enzyme/enzyme  — executable binary (copied from plugin)
#   ~/.cache/enzyme/models  — symlink to plugin's bundled model files
#   ~/.local/bin/enzyme     — wrapper that sets ENZYME_MODEL_DIR and execs binary
set -e

PLUGIN_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# Skip only for Homebrew installs — they manage their own updates.
# All other cases (plugin-managed, stale cache) proceed to ensure
# the latest plugin binary is deployed.
if command -v enzyme >/dev/null 2>&1; then
    case "$(command -v enzyme)" in
        /opt/homebrew/*|/usr/local/*) exit 0 ;;
    esac
fi

# Detect platform.
OS=$(uname -s)
ARCH=$(uname -m)
case "${OS}-${ARCH}" in
    Darwin-arm64)   BIN="enzyme-darwin-arm64" ;;
    Linux-aarch64)  BIN="enzyme-linux-arm64"  ;;
    *)
        echo "enzyme: unsupported platform ${OS}-${ARCH}" >&2
        exit 0
        ;;
esac

SRC="${PLUGIN_ROOT}/bin/${BIN}"
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

# Symlink model files into the cache dir.
ln -sfn "${PLUGIN_ROOT}/models" "${CACHE_DIR}/models"

# Create a wrapper in PATH that uses ~/.cache-relative paths.
mkdir -p "$HOME/.local/bin"
cat > "$HOME/.local/bin/enzyme" << WRAPPER
#!/bin/sh
export ENZYME_MODEL_DIR="\$HOME/.cache/enzyme/models"
exec "\$HOME/.cache/enzyme/enzyme" "\$@"
WRAPPER
chmod +x "$HOME/.local/bin/enzyme"

# Ensure ~/.local/bin is in PATH for all subsequent Bash tool calls.
if [ -n "$CLAUDE_ENV_FILE" ]; then
    echo 'export PATH="$HOME/.local/bin:$PATH"' >> "$CLAUDE_ENV_FILE"
fi
