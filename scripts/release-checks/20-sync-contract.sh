#!/usr/bin/env bash
# Sync-contract checks: assert the invariants that keep the skill mirrors and
# installers from drifting. These encode past real incidents (skill mirrors
# drifting from the canonical copy, install.sh diverging across copies).
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

echo "sync-contract"

# Skill distribution boundaries and runtime mirrors
check "workspace skill mirror in sync" \
  python3 "$REPO_ROOT/scripts/sync-workspace-setup-skill.py" --check
check "agent runtime skill in sync" \
  python3 "$REPO_ROOT/scripts/sync-agent-runtime-skill.py" --check
check "source installers in sync" \
  cmp "$REPO_ROOT/install.sh" "$REPO_ROOT/agent/install.sh"
check "installers preserve existing ~/.enzyme data" \
  "$REPO_ROOT/scripts/test_installer_safety.sh"

# install.sh parity with the website copy, when ENZYME_SITE_INSTALL points at
# it. The site copy carries a usage-comment header; compare bodies from the
# first non-comment line.
if [ -n "${ENZYME_SITE_INSTALL:-}" ] && [ -f "$ENZYME_SITE_INSTALL" ]; then
  check "install.sh parity: $ENZYME_SITE_INSTALL" \
    bash -c "diff <(grep -v '^#' '$REPO_ROOT/install.sh') <(grep -v '^#' '$ENZYME_SITE_INSTALL')"
fi

report "sync-contract"
