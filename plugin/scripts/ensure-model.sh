#!/usr/bin/env bash
# Ensure the Enzyme embedding model is installed.
# Called by the SessionStart hook — non-blocking.

# Check if enzyme is on PATH
if ! command -v enzyme &>/dev/null; then
  echo "enzyme: not found on PATH. Install from https://github.com/jshph/enzyme-rust" >&2
  exit 0
fi

# Check if model is present
if enzyme setup --check 2>/dev/null; then
  exit 0
fi

# Model missing — download it
echo "enzyme: downloading embedding model..." >&2
enzyme setup 2>&1
