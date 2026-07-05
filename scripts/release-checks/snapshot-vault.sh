#!/usr/bin/env bash
# Snapshot the real vault's markdown corpus into a local test directory.
#
# Real vault data never enters git or leaves this machine. The snapshot is
# markdown-only (no media, no .enzyme state) so lifecycle checks run against
# realistic content at realistic scale.
#
# Usage: snapshot-vault.sh [source-vault] [dest-dir]
#   source-vault  defaults to $ENZYME_REAL_VAULT or ~/obsidian
#   dest-dir      defaults to ~/.enzyme-test-vaults/real
set -euo pipefail

SRC="${1:-${ENZYME_REAL_VAULT:-$HOME/obsidian}}"
DEST="${2:-$HOME/.enzyme-test-vaults/real}"

if [ ! -d "$SRC" ]; then
  echo "snapshot-vault: source vault not found: $SRC" >&2
  exit 1
fi

mkdir -p "$DEST"

rsync -a --delete \
  --prune-empty-dirs \
  --exclude='.*' \
  --exclude='.enzyme/' \
  --exclude='.obsidian/' \
  --exclude='.trash/' \
  --include='*/' \
  --include='*.md' \
  --exclude='*' \
  "$SRC/" "$DEST/"

count=$(find "$DEST" -name '*.md' | wc -l | tr -d ' ')
size=$(du -sh "$DEST" | cut -f1)
echo "snapshot-vault: $count markdown files ($size) at $DEST"
