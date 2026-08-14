#!/usr/bin/env bash
# Vault lifecycle checks against real markdown data.
#
# Tiers:
#   offline  (always)  — index-only init in a scrubbed environment, then
#                        status/doctor/catalyze/petri must degrade gracefully.
#                        Encodes the failure classes from the 2026-07 reliability
#                        sweep: empty-catalyst retrieval, malformed frontmatter,
#                        corrupt DB, uninitialized vault.
#   full     (default) — full init on the small canary fixture through the
#                        loginless hosted bootstrap (the memory.enzyme.garden cold-start
#                        path). Asserts embeddings, catalysts, canary retrieval,
#                        and incremental refresh. Consumes a small amount of
#                        hosted bootstrap credits; skip with ENZYME_CHECK_FULL=0.
#
# Vault selection:
#   ENZYME_TEST_VAULT — additionally run the offline tier against this vault
#   (e.g. a snapshot-vault.sh output) for realistic scale. The full tier always
#   uses the fixture so cost and runtime stay bounded.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
require_bin

FIXTURE="$REPO_ROOT/tests/fixtures/canary-vault"
OUT=$(mktemp "${TMPDIR:-/tmp}/enzyme-lifecycle-out.XXXXXX")
trap 'rm -f "$OUT"' EXIT

# A working copy of a vault in a scrubbed, isolated environment.
# Sets: WORK_VAULT, ISO_HOME
setup_vault_copy() {
  local src="$1"
  local base
  base=$(mktemp -d "${TMPDIR:-/tmp}/enzyme-lifecycle.XXXXXX")
  WORK_VAULT="$base/vault"
  ISO_HOME="$base/home"
  mkdir -p "$ISO_HOME"
  cp -R "$src" "$WORK_VAULT"
}

# Run enzyme with isolated HOME + ENZYME_HOME and no ambient LLM credentials.
# ENZYME_HOME isolation matters: auth.json/bootstrap.json live there, not
# under $HOME, so HOME alone does not isolate credential state.
enz() {
  env -u OPENAI_API_KEY -u OPENAI_BASE_URL -u OPENAI_MODEL \
      -u OPENROUTER_API_KEY -u OPENROUTER_BASE_URL -u OPENROUTER_MODEL \
      HOME="$ISO_HOME" ENZYME_HOME="$ISO_HOME/.enzyme" \
      "$ENZYME_BIN" "$@"
}

# run_check <desc> [--expect <substring>] [--may-fail] <enzyme args...>
# Runs `enz` capturing output to $OUT. Fails on panic always; fails on
# non-zero exit unless --may-fail; fails if --expect substring is absent.
run_check() {
  local desc="$1"; shift
  local expect="" may_fail=0
  while [[ "${1:-}" == --* ]]; do
    case "$1" in
      --expect) expect="$2"; shift 2 ;;
      --may-fail) may_fail=1; shift ;;
    esac
  done
  local rc=0
  enz "$@" >"$OUT" 2>&1 || rc=$?
  local why=""
  if grep -q "panicked" "$OUT"; then
    why="panicked"
  elif [ "$rc" -ne 0 ] && [ "$may_fail" -eq 0 ]; then
    why="exit $rc"
  elif [ -n "$expect" ] && ! grep -qF "$expect" "$OUT"; then
    why="missing expected output: $expect"
  fi
  if [ -z "$why" ]; then
    PASS=$((PASS + 1)); echo "  ok   $desc"
  else
    FAIL=$((FAIL + 1)); FAILED_CHECKS+=("$desc")
    echo "  FAIL $desc ($why)" >&2
    head -5 "$OUT" | sed 's/^/       /' >&2
  fi
}

offline_tier() {
  local src="$1" label="$2"
  setup_vault_copy "$src"
  echo "offline tier: $label"

  run_check "$label: doctor on uninitialized vault does not panic" --may-fail doctor --json -p "$WORK_VAULT"
  run_check "$label: init --stage index" -p "$WORK_VAULT" init --stage index --quiet
  check "$label: db created" test -f "$WORK_VAULT/.enzyme/enzyme.db"
  run_check "$label: status" -p "$WORK_VAULT" status
  run_check "$label: doctor --json" -p "$WORK_VAULT" doctor --json
  check "$label: doctor output is valid JSON" python3 -m json.tool "$OUT"

  # Index-only vault has no catalysts: catalyze/petri must exit 0 gracefully
  # (current contract is an empty result set, not a fallback search).
  run_check "$label: catalyze with no catalysts is graceful" \
    --expect '"results":[]' -p "$WORK_VAULT" catalyze "anything at all"
  run_check "$label: petri with no catalysts exits 0" -p "$WORK_VAULT" petri

  # Corrupt DB: commands may fail, but never panic
  echo "garbage not a sqlite file" > "$WORK_VAULT/.enzyme/enzyme.db"
  run_check "$label: doctor on corrupt db does not panic" --may-fail -p "$WORK_VAULT" doctor --json
  run_check "$label: status on corrupt db does not panic" --may-fail -p "$WORK_VAULT" status
}

fixture_offline_extras() {
  # Fixture-specific facts (counts, malformed-YAML detection) that only hold
  # on the canary vault.
  setup_vault_copy "$FIXTURE"
  enz -p "$WORK_VAULT" init --stage index --quiet >/dev/null 2>&1
  run_check "fixture: doctor sees all 6 docs" \
    --expect '"docs_in_db": 6' -p "$WORK_VAULT" doctor --json
  run_check "fixture: doctor flags malformed frontmatter" \
    --expect 'malformed-frontmatter.md' -p "$WORK_VAULT" doctor --json
}

full_tier() {
  setup_vault_copy "$FIXTURE"
  echo "full tier: canary fixture via loginless hosted bootstrap"

  run_check "full: init via hosted bootstrap" -p "$WORK_VAULT" init --quiet
  run_check "full: all docs embedded" --expect 'Embedded:   6/6' -p "$WORK_VAULT" status
  if grep -q 'Catalysts:  0' "$OUT"; then
    FAIL=$((FAIL + 1)); FAILED_CHECKS+=("full: catalysts generated")
    echo "  FAIL full: catalysts generated (count is 0)" >&2
  else
    PASS=$((PASS + 1)); echo "  ok   full: catalysts generated"
  fi

  run_check "full: canary retrieval (antenna -> heliograph note)" \
    --expect 'heliograph' -p "$WORK_VAULT" catalyze "antenna mounting decision"
  run_check "full: petri has entities" --expect '"name"' -p "$WORK_VAULT" petri

  # Incremental refresh picks up a new note
  cat > "$WORK_VAULT/projects/new-note.md" <<'EOF'
---
title: Refresh canary
tags: [projects]
date: 2026-06-30
---
# Refresh canary
The CANARY-REFRESH11 beacon arrived and pairs with the heliograph array.
EOF
  run_check "full: refresh after adding note" -p "$WORK_VAULT" refresh --quiet
  run_check "full: refreshed doc visible in doctor" \
    --expect '"docs_in_db": 7' -p "$WORK_VAULT" doctor --json
}

echo "vault-lifecycle (binary: $ENZYME_BIN)"

offline_tier "$FIXTURE" "fixture"
fixture_offline_extras

if [ -n "${ENZYME_TEST_VAULT:-}" ]; then
  if [ -d "$ENZYME_TEST_VAULT" ]; then
    offline_tier "$ENZYME_TEST_VAULT" "real-vault"
  else
    echo "  skip real-vault tier: ENZYME_TEST_VAULT=$ENZYME_TEST_VAULT not found" >&2
  fi
else
  echo "  note: set ENZYME_TEST_VAULT (see snapshot-vault.sh) to run the offline tier at real scale"
fi

if [ "${ENZYME_CHECK_FULL:-1}" = "1" ]; then
  full_tier
else
  echo "  skip full tier (ENZYME_CHECK_FULL=0)"
fi

report "vault-lifecycle"
