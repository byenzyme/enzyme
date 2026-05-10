---
name: enzyme
description: >
  Set up Enzyme for an uninitialized Obsidian or Markdown vault. Use when the
  vault needs first-time scan, user-confirmed entity selection, config
  persistence, or optional frontmatter backfill planning.
allowed-tools: Bash, Read, Glob, Grep, Edit, Write
argument-hint: [vault-path]
---

# Enzyme Setup

Use this skill only for first-time vault setup. If the vault is already initialized, say so and stop; ordinary vault exploration should use the regular Enzyme tools and runtime hooks, not this slash skill.

## Vault Path

Enzyme resolves the vault path in this order: `-p` flag > `ENZYME_VAULT_ROOT` env var > current directory.

Before setup, resolve the vault root once and run the workflow from that directory. If the user passed a vault path, `cd` there first or export it as `ENZYME_VAULT_ROOT`; the persistent instruction writer uses that same root.

Check initialization:

```bash
ls /tmp/enzyme/enzyme.db ${ENZYME_VAULT_ROOT:-.}/.enzyme/enzyme.db 2>/dev/null
```

If `enzyme.db` exists, the vault is initialized. Do not run the setup workflow.

## Commands

```bash
enzyme scan                 # Preview suggested entities and exclusions
enzyme scan --write-config  # Persist scan suggestions after user confirmation
enzyme init --quiet         # Initialize after confirmed config is persisted
nohup enzyme login --json --no-open > /tmp/enzyme-login.log 2>&1 & # Agent-mediated login; show the URL from the log
enzyme status               # Check whether the vault is initialized
```

Read Enzyme JSON directly. Do not pipe through Python or jq unless the output is malformed or too large to inspect.

## Setup Workflow

1. **Run scan preview.** Run `enzyme scan` before init. Treat it as a proposal, not truth. Do not run `enzyme scan --write-config` yet.

2. **Handle login if needed.** If there is no API key in the environment and no `~/.enzyme/auth.json`, start login in the background:

   ```bash
   nohup enzyme login --json --no-open > /tmp/enzyme-login.log 2>&1 &
   echo "Started login PID $!"
   sleep 3
   cat /tmp/enzyme-login.log
   ```

   Read JSONL events directly:
   - `already_logged_in`: continue setup.
   - `device_authorization`: show the `verification_uri` to the user, leave the background process running, and re-check `/tmp/enzyme-login.log` until it emits `success`, `expired`, or `error`.
   - `success`: continue setup.
   - `expired` or `error`: explain the failure and rerun login only if the user wants to try again.

   Do not ask the user to paste API keys. Do not stop after showing the URL; keep checking the background login log until it emits `success`, fails, or expires.

3. **Audit independently.** Inspect the vault yourself:
   - Folders: count markdown files by top-level and second-level folder; note recent activity and obvious structural folders.
   - Tags: count frontmatter and inline tags, prioritizing recently modified files.
   - Links: identify hub wikilinks and page-like entities, especially `people/` style folders.
   - Structural files: read `CLAUDE.md`, `AGENTS.md`, `README.md`, `MOC.md`, `Index.md`, and `_index.md` when present for folder/tag conventions only.
   - Frontmatter: sample notes for `created:`, `date:`, `people:`, `aliases:`, and topical `tags:`. Count which notes have frontmatter dates, filename/path dates, or only filesystem dates.

4. **Compare scan vs audit.** Before writing anything, show the user:
   - What `enzyme scan` proposed.
   - What your audit confirmed.
   - What your audit found that scan missed.
   - What scan suggested that looks structural, stale, accidental, or risky.
   - The exact entities and exclusions that would be persisted.

   Ask the user to confirm or correct the list. This confirmation is required before config is written.

5. **Persist confirmed config.** After confirmation, run:

   ```bash
   enzyme scan --write-config
   ```

   If the user corrected the list, edit `~/.enzyme/config.toml` so this vault's section matches the confirmed config. Do not write `guide.md`; vault-local guide setup is outdated.

   Config shape:

   ```toml
   [vaults."/path/to/vault"]
   entities = ["#research", "#writing", "folder:people", "folder:projects", "[[open questions]]"]
   excluded_tags = ["todo", "template", "archived"]
   excluded_folders = [".claude", ".enzyme", ".git", ".obsidian", "templates"]
   ```

6. **Initialize.** Run:

   ```bash
   enzyme init --quiet
   ```

   The quiet output includes petri data under `petri`; do not run a separate `enzyme petri` call during setup.

7. **Install persistent agent instructions.** `enzyme init --quiet` does not install agent-facing instructions, so app-mediated setup must do it explicitly after init. Use the command for the active agent:

   ```bash
   enzyme agents codex
   # or
   enzyme agents claude
   ```

   These commands fetch the latest instruction templates from GitHub when network access is available, fall back to the packaged copy otherwise, update only the marked Enzyme section in `AGENTS.md`, and install the runtime skill into the agent-specific skill directory.

8. **Optional frontmatter backfill.** If the audit found useful missing structure, propose a separate script and show exactly what it would change before running it. Good candidates:
   - Add `created: '[[YYYY-MM-DD]]'` when a file has a date in its filename/path but no frontmatter date.
   - Add `people:` wikilinks to synced emails/transcripts by matching existing `people/*.md` notes.
   - Create missing `people/<Name>.md` stubs only when names are repeatedly referenced and the user confirms.

   Never backfill frontmatter during first init unless the user explicitly approves the script and scope.

9. **Report setup result.** Tell the user the initialized note count, the persisted entity list, exclusions, that `AGENTS.md` was updated and `CLAUDE.md` imports it, and any frontmatter backfill you intentionally left for a separate confirmation.
