<!-- enzyme:start -->

## Enzyme Workspace Context

This workspace uses Enzyme for local semantic retrieval over markdown. Run `enzyme` commands from this workspace root unless the user gives another vault path.

The installed skills are the source of truth:

- For setup, re-setup, diagnosis, or repair, read the workspace setup skill first:
  - Claude Code: `.claude/skills/enzyme-workspace-setup/SKILL.md`
  - Codex / Pi / generic agents: `.agents/skills/enzyme-workspace-setup/SKILL.md`
- For normal retrieval, refresh, and note-writing after setup, read the installed runtime skill:
  - Claude Code: `.claude/skills/enzyme/SKILL.md`
  - Codex / Pi / generic agents: `.agents/skills/enzyme/SKILL.md`
- On `enzyme refresh --quiet`, check `update.status` in the JSON: `ok` means nothing to do; `action_required` means perform or relay the `action` string.

Do not treat `.enzyme/`, `AGENTS.md`, `CLAUDE.md`, or installed skill files as proof that setup is complete or healthy. They are evidence to inspect through the setup skill.

If an Enzyme instruction here conflicts with an installed skill, follow the installed skill.

<!-- enzyme:end -->
