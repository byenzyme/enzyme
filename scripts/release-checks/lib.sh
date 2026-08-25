#!/usr/bin/env bash
# Shared helpers for release checks. Source this; do not execute.

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

default_enzyme_bin() {
  local target_dir
  if target_dir="$(
    cd "$REPO_ROOT" &&
      cargo metadata --format-version=1 --no-deps 2>/dev/null |
        python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])'
  )"; then
    printf '%s/debug/enzyme\n' "$target_dir"
  else
    printf '%s/target/debug/enzyme\n' "$REPO_ROOT"
  fi
}

# Binary under test. Override with ENZYME_BIN; defaults to the debug build.
ENZYME_BIN="${ENZYME_BIN:-$(default_enzyme_bin)}"

PASS=0
FAIL=0
FAILED_CHECKS=()

check() {
  local desc="$1"
  shift
  if "$@" >/dev/null 2>&1; then
    PASS=$((PASS + 1))
    echo "  ok   $desc"
  else
    FAIL=$((FAIL + 1))
    FAILED_CHECKS+=("$desc")
    echo "  FAIL $desc" >&2
  fi
}

# check_output <desc> <expected-substring> <command...>
check_output() {
  local desc="$1" expect="$2"
  shift 2
  local out
  if out=$("$@" 2>&1) && [[ "$out" == *"$expect"* ]]; then
    PASS=$((PASS + 1))
    echo "  ok   $desc"
  else
    FAIL=$((FAIL + 1))
    FAILED_CHECKS+=("$desc")
    echo "  FAIL $desc" >&2
    echo "       expected substring: $expect" >&2
    echo "       got: $(echo "$out" | head -5)" >&2
  fi
}

isolated_home() {
  mktemp -d "${TMPDIR:-/tmp}/enzyme-check-home.XXXXXX"
}

require_bin() {
  if [ ! -x "$ENZYME_BIN" ]; then
    echo "release-checks: enzyme binary not found at $ENZYME_BIN" >&2
    echo "run: scripts/cargo-lane shared -- cargo build -p enzyme-cli --features local-llm   (or set ENZYME_BIN)" >&2
    exit 1
  fi
}

report() {
  local name="$1"
  echo
  echo "$name: $PASS passed, $FAIL failed"
  if [ "$FAIL" -gt 0 ]; then
    printf '  - %s\n' "${FAILED_CHECKS[@]}" >&2
    exit 1
  fi
}
