#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/test_codex_skill_install.sh [--run-codex] [--keep]

Creates a temporary workspace and temporary HOME/CODEX_HOME, installs the Enzyme
Codex runtime skill with `enzyme install codex --source local`, verifies the
installed files, optionally runs `codex exec` against the isolated workspace,
and cleans up unless --keep is passed.

Environment:
  ENZYME_BIN       Path to enzyme binary. Defaults to target/debug/enzyme,
                   building it if missing.
  SOURCE_CODEX_HOME Source auth/config directory. Defaults to $CODEX_HOME or ~/.codex.
USAGE
}

run_codex=0
keep=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --run-codex) run_codex=1 ;;
    --keep) keep=1 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
  shift
done

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

if [[ -z "${ENZYME_BIN:-}" ]]; then
  cargo build -p enzyme-cli
  ENZYME_BIN="$repo_root/target/debug/enzyme"
fi

if [[ ! -x "$ENZYME_BIN" ]]; then
  echo "enzyme binary not executable: $ENZYME_BIN" >&2
  exit 1
fi

if ! command -v codex >/dev/null 2>&1; then
  echo "codex CLI not found on PATH" >&2
  exit 1
fi

tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/enzyme-codex-skill-test.XXXXXX")"
cleanup() {
  if [[ "$keep" -eq 1 ]]; then
    echo "kept test directory: $tmp_root" >&2
  else
    rm -rf "$tmp_root"
  fi
}
trap cleanup EXIT

workspace="$tmp_root/workspace"
test_home="$tmp_root/home"
test_codex_home="$tmp_root/codex-home"
mkdir -p "$workspace/notes" "$test_home" "$test_codex_home"
chmod 700 "$test_codex_home"

cat > "$workspace/notes/example.md" <<'MD'
# Example note

Enzyme should preserve the user's existing markdown structure and retrieve context from it.
MD

# Keep Codex state isolated, but copy enough auth/config to allow an optional
# non-interactive smoke test without writing to the user's real ~/.codex.
source_codex_home="${SOURCE_CODEX_HOME:-${CODEX_HOME:-$HOME/.codex}}"
if [[ -f "$source_codex_home/auth.json" ]]; then
  cp "$source_codex_home/auth.json" "$test_codex_home/auth.json"
fi
if [[ -f "$source_codex_home/config.toml" ]]; then
  cp "$source_codex_home/config.toml" "$test_codex_home/config.toml"
fi

HOME="$test_home" CODEX_HOME="$test_codex_home" \
  "$ENZYME_BIN" -p "$workspace" install codex --source local

skill_path="$workspace/.agents/skills/enzyme/SKILL.md"
agents_path="$workspace/AGENTS.md"

test -f "$skill_path"
test -f "$agents_path"
grep -q '^name: enzyme$' "$skill_path"
grep -q 'enzyme scan' "$skill_path"
grep -q 'do not call `enzyme install <runtime>`' "$skill_path"
grep -q '<!-- enzyme:start -->' "$agents_path"
grep -q '<!-- enzyme:end -->' "$agents_path"

echo "install verification passed"
echo "workspace: $workspace"
echo "skill: $skill_path"

if [[ "$run_codex" -eq 1 ]]; then
  if [[ ! -f "$test_codex_home/auth.json" ]]; then
    echo "cannot run codex smoke: no auth.json copied from $source_codex_home" >&2
    exit 1
  fi

  output_file="$tmp_root/codex-final.txt"
  prompt='Use the enzyme skill if it is available. Do not run shell commands. Answer exactly: ENZYME_SKILL_OK command=<the first setup command shown in the Enzyme skill first-time setup block>'

  HOME="$test_home" CODEX_HOME="$test_codex_home" \
    codex exec \
      --cd "$workspace" \
      --skip-git-repo-check \
      --ephemeral \
      --ignore-rules \
      --sandbox read-only \
      --output-last-message "$output_file" \
      "$prompt"

  if ! grep -q 'ENZYME_SKILL_OK command=enzyme scan' "$output_file"; then
    echo "codex smoke failed; final answer was:" >&2
    cat "$output_file" >&2
    exit 1
  fi
  echo "codex smoke passed"
fi
