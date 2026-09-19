#!/usr/bin/env bash
# Sync-contract checks: assert the invariants that keep the source repo,
# plugin payload, and downstream repos from drifting. These encode past
# real incidents (stale hooks.json shipping in the public plugin payload,
# skill mirrors drifting from the canonical copy, install.sh diverging
# across repos).
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

echo "sync-contract"

# Skill distribution boundaries and runtime mirrors
check "workspace skill mirror in sync" \
  python3 "$REPO_ROOT/scripts/sync-workspace-setup-skill.py" --check
check "agent runtime skill in sync" \
  python3 "$REPO_ROOT/scripts/sync-agent-runtime-skill.py" --check
check "public README mirror in sync" \
  python3 "$REPO_ROOT/scripts/sync-public-readme.py" --check
check "source installers in sync" \
  cmp "$REPO_ROOT/plugin/install.sh" "$REPO_ROOT/plugin/agent/install.sh"
check "installers preserve existing ~/.enzyme data" \
  "$REPO_ROOT/scripts/test_installer_safety.sh"

# No lifecycle hook files or hook claims anywhere in the plugin payload
hooks_found=$(find "$REPO_ROOT/plugin/plugins/enzyme" -name 'hooks.json' | wc -l | tr -d ' ')
check "no hooks.json in plugin payload" test "$hooks_found" -eq 0

for pj in "$REPO_ROOT"/plugin/plugins/enzyme/.claude-plugin/plugin.json \
          "$REPO_ROOT"/plugin/plugins/enzyme/.codex-plugin/plugin.json; do
  if [ -f "$pj" ]; then
    check "no hooks field in ${pj#"$REPO_ROOT"/}" \
      bash -c "! python3 -c 'import json,sys; sys.exit(0 if \"hooks\" in json.load(open(\"$pj\")) else 1)'"
    check "valid JSON: ${pj#"$REPO_ROOT"/}" python3 -m json.tool "$pj"
  fi
done

# install.sh parity with sibling repos, when they are checked out locally.
# Checkout roots differ per machine, so search the known layouts rather than
# only the macOS one; a path that matches nothing skips silently and would let
# the copies drift unnoticed.
sibling_candidates=()
for root in "$HOME/Hacks" /workspace/projects; do
  sibling_candidates+=("$root/enzyme/install.sh" "$root/enzyme-site/public/install.sh")
done
for sibling in "${sibling_candidates[@]}"; do
  if [ -f "$sibling" ]; then
    # The site copy carries a usage-comment header; compare bodies from the
    # first non-comment line.
    check "install.sh parity: $sibling" \
      bash -c "diff <(grep -v '^#' '$REPO_ROOT/plugin/install.sh') <(grep -v '^#' '$sibling')"
  fi
done

report "sync-contract"
