#!/usr/bin/env bash
# Agentic release checks: drive a real headless Claude Code agent through the
# two setup journeys users actually take, and grade with deterministic
# assertions.
#
#   cold-start — agent is pointed at https://memory.enzyme.garden in a fresh sandbox
#                and must install enzyme and set up a markdown workspace.
#                Exercises the public site copy, install.sh, the released
#                binary, and loginless hosted bootstrap end to end.
#   teardown   — agent gets a workspace with an existing enzyme setup and must
#                clear it completely without damaging user content.
#
# Auth: the claude CLI authenticates via the real HOME (Keychain), so the
# agent process keeps the real HOME. Every command the agent runs is told to
# use the sandbox HOME/ENZYME_HOME instead, and a checksum guardrail fails the
# check if the real ~/.local/bin/enzyme was touched.
#
# Cost/flakiness: one agent run per scenario, using the local claude
# subscription. Run at release time or nightly, not per-commit.
# Skip with ENZYME_CHECK_AGENTIC=0. Transcripts land in
# .release-check-artifacts/ (gitignored).
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

if [ "${ENZYME_CHECK_AGENTIC:-1}" != "1" ]; then
  echo "agentic-setup: skipped (ENZYME_CHECK_AGENTIC=0)"
  exit 0
fi
if ! command -v claude >/dev/null 2>&1; then
  echo "agentic-setup: skipped (claude CLI not found)" >&2
  exit 0
fi

ARTIFACTS="$REPO_ROOT/.release-check-artifacts"
mkdir -p "$ARTIFACTS"
AGENT_MODEL="${ENZYME_AGENTIC_MODEL:-claude-sonnet-4-6}"

# Guardrail: prompt-level sandboxing is best-effort (agents have been observed
# running install.sh against the real HOME despite instructions), so back up
# the real binary and restore it if an agent run replaces it. An escape is
# reported as a loud warning, not a failure — the release signal is the
# journey assertions, not agent sandbox compliance. Note: tar-extracted
# binaries keep their build mtime, so mtime cannot detect this; checksums can.
real_bin="$HOME/.local/bin/enzyme"
real_bin_sum=""
real_bin_backup=""
if [ -f "$real_bin" ]; then
  real_bin_sum=$(shasum -a 256 "$real_bin" | cut -d' ' -f1)
  real_bin_backup=$(mktemp "${TMPDIR:-/tmp}/enzyme-bin-backup.XXXXXX")
  cp "$real_bin" "$real_bin_backup"
fi

guardrail_check() {
  local label="$1"
  [ -n "$real_bin_sum" ] || return 0
  local now
  now=$(shasum -a 256 "$real_bin" 2>/dev/null | cut -d' ' -f1)
  if [ "$now" = "$real_bin_sum" ]; then
    echo "  ok   $label: real ~/.local/bin/enzyme untouched"
  else
    cp "$real_bin_backup" "$real_bin"
    chmod +x "$real_bin"
    echo "  WARN $label: agent escaped the sandbox and replaced ~/.local/bin/enzyme; restored from backup" >&2
    echo "       review $ARTIFACTS/agentic-$label.txt and check ~/.local/bin/lib for stray writes" >&2
  fi
}

run_agent() {
  local label="$1" workdir="$2" prompt="$3"
  local transcript="$ARTIFACTS/agentic-$label.txt"
  echo "  running agent: $label (model $AGENT_MODEL, transcript: $transcript)"
  ( cd "$workdir" && claude -p "$prompt" \
      --model "$AGENT_MODEL" \
      --permission-mode bypassPermissions \
      --no-session-persistence \
    ) >"$transcript" 2>&1
  local rc=$?
  check "$label: agent run completed" test "$rc" -eq 0
}

echo "agentic-setup"

# ---------------------------------------------------------------- cold-start
sandbox=$(mktemp -d "${TMPDIR:-/tmp}/enzyme-agentic.XXXXXX")
ws="$sandbox/workspace"
shome="$sandbox/home"
mkdir -p "$shome"
cp -R "$REPO_ROOT/tests/fixtures/canary-vault" "$ws"

run_agent "cold-start" "$ws" "You are validating Enzyme's new-user setup path.

Sandbox rules (mandatory): prefix EVERY shell command you run with
HOME=$shome ENZYME_HOME=$shome/.enzyme and use PATH=$shome/.local/bin:\$PATH
where needed. This applies especially to any 'curl ... | bash' installer —
run it as 'curl ... | HOME=$shome bash', never bare, or it will write to the
real home directory. Before each command that installs or writes, confirm the
HOME override is present in that exact command line. Never install to or
modify anything outside $sandbox and this workspace.

Task: this workspace ($ws) is a markdown notes vault with no Enzyme setup.
Fetch https://memory.enzyme.garden and follow its published instructions to install
the enzyme CLI and set up this workspace, exactly as a new user would. Do not
use any local checkout or prior knowledge of undocumented flags; rely on what
the site and the installed tool tell you. Finish when 'enzyme status' (run
with the sandbox env) reports an initialized vault for this workspace, then
print a short report of what you did."

check "cold-start: enzyme binary installed in sandbox" test -x "$shome/.local/bin/enzyme"
check "cold-start: runtime skill installed" bash -c \
  "test -f '$ws/.claude/skills/enzyme/SKILL.md' || test -f '$ws/.agents/skills/enzyme/SKILL.md'"
check "cold-start: workspace setup skill installed" bash -c \
  "test -f '$ws/.claude/skills/enzyme-workspace-setup/SKILL.md' || test -f '$ws/.agents/skills/enzyme-workspace-setup/SKILL.md'"
check "cold-start: workspace instructions route setup skill" \
  grep -q "enzyme-workspace-setup/SKILL.md" "$ws/AGENTS.md"
check "cold-start: vault db created" test -f "$ws/.enzyme/enzyme.db"
check "cold-start: status reports initialized vault" \
  env HOME="$shome" ENZYME_HOME="$shome/.enzyme" "$shome/.local/bin/enzyme" -p "$ws" status
guardrail_check "cold-start"

# ----------------------------------------------------------------- teardown
require_bin
sandbox2=$(mktemp -d "${TMPDIR:-/tmp}/enzyme-agentic.XXXXXX")
ws2="$sandbox2/workspace"
shome2="$sandbox2/home"
mkdir -p "$shome2"
cp -R "$REPO_ROOT/tests/fixtures/canary-vault" "$ws2"
HOME="$shome2" ENZYME_HOME="$shome2/.enzyme" "$ENZYME_BIN" -p "$ws2" install claude --source local >/dev/null 2>&1
HOME="$shome2" ENZYME_HOME="$shome2/.enzyme" "$ENZYME_BIN" -p "$ws2" init --stage index --quiet >/dev/null 2>&1
printf '\n# User notes\nKeep this line: SENTINEL-KEEP-ME\n' >> "$ws2/CLAUDE.md"
printf '\n# User agent notes\nKeep this line: AGENTS-SENTINEL-KEEP-ME\n' >> "$ws2/AGENTS.md"

run_agent "teardown" "$ws2" "This workspace ($ws2) has an existing Enzyme
setup: index state under .enzyme/, Enzyme skills under .claude/skills/, an
enzyme-managed section in AGENTS.md delimited by '<!-- enzyme:start -->' and
'<!-- enzyme:end -->', and a CLAUDE.md import of AGENTS.md.

Remove the Enzyme setup completely: delete the index state and the installed
Enzyme skills, remove only the enzyme-managed section from AGENTS.md, and
remove the Enzyme import from CLAUDE.md only if it is no longer needed.
Preserve all other user content in AGENTS.md, CLAUDE.md, and the vault notes
untouched. Work only inside $ws2. Print a short report of what you removed."

check "teardown: .enzyme state removed" bash -c "test ! -e '$ws2/.enzyme'"
check "teardown: runtime skill removed" bash -c "test ! -e '$ws2/.claude/skills/enzyme'"
check "teardown: workspace setup skill removed" bash -c "test ! -e '$ws2/.claude/skills/enzyme-workspace-setup'"
check "teardown: enzyme section gone from AGENTS.md" \
  bash -c "! grep -q 'enzyme:start' '$ws2/AGENTS.md'"
check "teardown: AGENTS.md user content preserved" grep -q "AGENTS-SENTINEL-KEEP-ME" "$ws2/AGENTS.md"
check "teardown: CLAUDE.md user content preserved" grep -q "SENTINEL-KEEP-ME" "$ws2/CLAUDE.md"
check "teardown: vault notes untouched" \
  test -f "$ws2/projects/heliograph-antenna.md"
guardrail_check "teardown"

report "agentic-setup"
