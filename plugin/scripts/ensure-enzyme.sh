#!/usr/bin/env bash
# Bootstrap enzyme: install binary + download embedding model.
# Called by the SessionStart hook — non-blocking, async.

set -euo pipefail

ENZYME_HOME="${HOME}/.enzyme"
BIN_DIR="${ENZYME_HOME}/bin"
ENZYME_BIN="${BIN_DIR}/enzyme"
RELEASE_REPO="jshph/enzyme"

# ── Helpers ──────────────────────────────────────────────────────────

detect_target() {
  local os arch
  os="$(uname -s | tr '[:upper:]' '[:lower:]')"
  arch="$(uname -m)"

  case "${os}-${arch}" in
    darwin-arm64)   echo "aarch64-apple-darwin" ;;
    darwin-x86_64)  echo "x86_64-apple-darwin" ;;
    linux-x86_64)   echo "x86_64-unknown-linux-gnu" ;;
    linux-aarch64)  echo "aarch64-unknown-linux-gnu" ;;
    *) echo "enzyme: unsupported platform ${os}-${arch}" >&2; return 1 ;;
  esac
}

latest_version() {
  # Fetch latest release tag from public repo (no auth needed)
  curl -fsSL "https://api.github.com/repos/${RELEASE_REPO}/releases/latest" \
    | grep -o '"tag_name": *"[^"]*"' \
    | head -1 \
    | sed 's/.*"tag_name": *"//;s/"//'
}

install_binary() {
  local target="$1"
  local version="$2"
  local url="https://github.com/${RELEASE_REPO}/releases/download/${version}/enzyme-${target}.tar.gz"

  echo "enzyme: installing ${version} for ${target}..." >&2

  mkdir -p "${BIN_DIR}"
  curl -fsSL "${url}" | tar xz -C "${BIN_DIR}"
  chmod +x "${ENZYME_BIN}"

  echo "enzyme: installed to ${ENZYME_BIN}" >&2
}

# ── Main ─────────────────────────────────────────────────────────────

# Resolve enzyme binary: prefer PATH, fall back to ~/.enzyme/bin
enzyme_cmd=""
if command -v enzyme &>/dev/null; then
  enzyme_cmd="enzyme"
elif [ -x "${ENZYME_BIN}" ]; then
  enzyme_cmd="${ENZYME_BIN}"
fi

# 1. Install binary if missing
if [ -z "${enzyme_cmd}" ]; then
  target="$(detect_target)" || exit 0
  version="$(latest_version)" || { echo "enzyme: could not fetch latest version" >&2; exit 0; }

  if [ -z "${version}" ]; then
    echo "enzyme: no releases found" >&2
    exit 0
  fi

  install_binary "${target}" "${version}" || { echo "enzyme: binary install failed" >&2; exit 0; }
  enzyme_cmd="${ENZYME_BIN}"
fi

# 2. Ensure embedding model is downloaded
if ! "${enzyme_cmd}" setup --check 2>/dev/null; then
  echo "enzyme: downloading embedding model..." >&2
  "${enzyme_cmd}" setup 2>&1
fi
