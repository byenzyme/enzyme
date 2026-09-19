#!/usr/bin/env bash
# Runtime install matrix: install the agent integration into isolated HOMEs
# for each supported runtime and assert the expected files land.
# Extracted from .agents/skills/release/SKILL.md step 10 so agents, humans,
# and CI all run the same code.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
require_bin

echo "install-matrix (binary: $ENZYME_BIN)"

# Claude runtime skill install
tmp_home=$(isolated_home)
tmp_workspace=$(mktemp -d)
HOME="$tmp_home" "$ENZYME_BIN" -p "$tmp_workspace" install claude --source local >/dev/null 2>&1
check "claude: SKILL.md installed" test -f "$tmp_workspace/.claude/skills/enzyme/SKILL.md"
check "claude: workspace setup SKILL.md installed" test -f "$tmp_workspace/.claude/skills/enzyme-workspace-setup/SKILL.md"
check "claude: shared practice review installed" test -f "$tmp_workspace/.claude/skills/enzyme-workspace-setup/references/knowledge-practice-review.md"
check "claude: setup skill includes scan/compile/proof path" grep -q "compile -v" "$tmp_workspace/.claude/skills/enzyme-workspace-setup/SKILL.md"
check "claude: CLAUDE.md written" test -f "$tmp_workspace/CLAUDE.md"

# Reinstall preserves surrounding CLAUDE.md content (marked-section contract)
echo "# my own notes" >> "$tmp_workspace/CLAUDE.md"
HOME="$tmp_home" "$ENZYME_BIN" -p "$tmp_workspace" install claude --source local >/dev/null 2>&1
check "claude: reinstall preserves user CLAUDE.md content" grep -q "my own notes" "$tmp_workspace/CLAUDE.md"

# Hermes
tmp_home=$(isolated_home)
tmp_workspace=$(mktemp -d)
tmp_hermes=$(mktemp -d)
HOME="$tmp_home" HERMES_HOME="$tmp_hermes" "$ENZYME_BIN" -p "$tmp_workspace" install hermes --source local >/dev/null 2>&1
check "hermes: SKILL.md installed" test -f "$tmp_hermes/skills/enzyme/SKILL.md"
check "hermes: workspace setup skill installed" test -f "$tmp_hermes/skills/enzyme-workspace-setup/SKILL.md"
check "hermes: shared practice review installed" test -f "$tmp_hermes/skills/enzyme-workspace-setup/references/knowledge-practice-review.md"

# OpenClaw
tmp_home=$(isolated_home)
tmp_workspace=$(mktemp -d)
tmp_openclaw=$(mktemp -d)
HOME="$tmp_home" OPENCLAW_HOME="$tmp_openclaw" "$ENZYME_BIN" -p "$tmp_workspace" install openclaw --source local >/dev/null 2>&1
check "openclaw: SKILL.md installed" test -f "$tmp_openclaw/skills/enzyme/SKILL.md"
check "openclaw: workspace setup skill installed" test -f "$tmp_openclaw/skills/enzyme-workspace-setup/SKILL.md"
check "openclaw: shared practice review installed" test -f "$tmp_openclaw/skills/enzyme-workspace-setup/references/knowledge-practice-review.md"

# Codex install path has its own script; it requires the codex CLI on PATH
# even in install-only mode, so skip (loudly) where codex is unavailable.
if command -v codex >/dev/null 2>&1; then
  check "codex: skill install script" \
    env ENZYME_BIN="$ENZYME_BIN" "$REPO_ROOT/scripts/test_codex_skill_install.sh"
else
  echo "  skip codex: skill install script (codex CLI not on PATH)"
fi

report "install-matrix"
