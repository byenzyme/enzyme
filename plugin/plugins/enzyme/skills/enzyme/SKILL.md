---
name: enzyme
description: >
  Set up Enzyme for an uninitialized Obsidian, Markdown, or Hermes agent
  workspace. Use when the workspace needs compatibility assessment, first-time
  scan, user-confirmed entity selection, TOML config validation, persistent
  agent instructions, or optional frontmatter backfill planning.
allowed-tools: Bash, Read, Glob, Grep, Edit, Write
argument-hint: [vault-path]
---

# Enzyme Setup

Use this skill only for first-time workspace setup. If the vault is already initialized, say so and stop; ordinary vault exploration should use the regular Enzyme tools and runtime hooks, not this slash skill.

For Hermes, this is an operational workspace setup path, not Hermes development. The goal is to leave the Hermes agent with a local Enzyme index and durable workspace instructions so future sessions retrieve context through Enzyme automatically.

Enzyme should bootstrap on top of the user's existing markdown system. Do not impose a memory schema or context tree. Treat existing Obsidian folders, inboxes, tags, wikilinks, daily notes, people pages, and frontmatter conventions as signal first; only propose changes when they make retrieval materially better and the user can review them. Prefer tags for recurring ideas and wikilinks for people, projects, companies, decisions, and concepts before creating folders or person-specific structures.

## Vault Path

Enzyme resolves the vault path in this order: `-p` flag > `ENZYME_VAULT_ROOT` env var > current directory.

Before setup, resolve the vault root once and run the workflow from that directory. If the user passed a vault path, `cd` there first or export it as `ENZYME_VAULT_ROOT`; the persistent instruction writer uses that same root.

For Hermes, prefer the directory where the user launches `hermes`. Hermes loads `.hermes.md`/`HERMES.md` by walking upward toward the git root and loads `AGENTS.md` from the current working directory at startup, so the workspace root matters.

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
enzyme install hermes       # Or openclaw/codex/claude for the active runtime
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
   - Folders: count markdown files by top-level and second-level folder; note recent activity and obvious structural folders. Pay special attention to existing Obsidian-style structure: `inbox`, `daily`, `journal`, `people`, `contacts`, `companies`, `projects`, `areas`, `resources`, `archive`, `meetings`, `decisions`, `research`, and `notes`.
   - Tags: count frontmatter and inline tags, prioritizing recently modified files.
   - Links: identify hub wikilinks and page-like entities, especially `people/`, `contacts/`, company, client, or CRM-style folders.
   - Structural files: read `CLAUDE.md`, `AGENTS.md`, `README.md`, `MOC.md`, `Index.md`, and `_index.md` when present for folder/tag conventions only.
   - Frontmatter: sample notes for `created:`, `date:`, `people:`, `organizations:`, `companies:`, `clients:`, `projects:`, `relationships:`, `aliases:`, and topical `tags:`. Count which notes have frontmatter dates, filename/path dates, date-like filenames or paths, or only filesystem timestamps. Preserve any existing date field name and entity field names when they are consistent.
   - People/CRM candidates: look for repeated person names in filenames, wikilinks, `people:` fields, meeting notes, call transcripts, email exports, and folders such as `people`, `contacts`, `clients`, `companies`, or `relationships`. If a people folder already exists, treat it as the canonical source. If it does not, prefer wikilinks and tags; only propose creating a people/company folder when repeated entities are clearly central to the workspace and the user confirms the convention.
   - Hermes fit: check whether this is a markdown-producing operational workspace, not just a source checkout. Good fits have durable notes, decision logs, research, transcripts, project journals, or agent-written summaries. Weak fits are mostly binary assets, generated build outputs, dependency folders, or code without markdown traces.

4. **Compare scan vs audit.** Before writing anything, show the user:
   - What `enzyme scan` proposed.
   - What your audit confirmed.
   - What your audit found that scan missed.
   - What scan suggested that looks structural, stale, accidental, or risky.
   - The exact entities and exclusions that would be persisted.
   - Any optional backfills that would improve retrieval, separated from required setup.

   Ask the user to confirm or correct the list. This confirmation is required before config is written.

5. **Persist confirmed config.** After confirmation, run:

   ```bash
   enzyme scan --write-config
   ```

   Then read `~/.enzyme/config.toml` directly. Do not assume the generated TOML is complete.

   Validate the `[vaults."/absolute/path"]` section against your audit:
   - Every important content folder with markdown files should appear as `folder:<path>` unless it is intentionally covered by a parent folder or excluded.
   - Existing Obsidian or Hermes folders that carry useful memory, such as `inbox`, `daily`, `journal`, `docs`, `notes`, `research`, `logs`, `decisions`, `meetings`, `transcripts`, `sessions`, `projects`, `areas`, `resources`, `people`, `contacts`, `clients`, and `companies`, should be added when present and non-empty.
   - Structural/runtime folders should remain excluded: `.claude`, `.conversations`, `.enzyme`, `.git`, `.hermes`, `.obsidian`, `.trash`, `node_modules`, `target`, `dist`, `build`, `__pycache__`, and templates.
   - If the generated section uses only tags/links and misses the workspace's main markdown folders, insert the missing `folder:<path>` entries.
   - Keep the entity list focused. Add folders that explain how the workspace is organized; do not add every tiny subfolder.
   - Treat recurring frontmatter entity fields as a vault convention, not as config entities. They should guide note-writing and optional backfills without bloating the `entities` list.

   If the user corrected the list, or the TOML missed crucial audited folders, edit `~/.enzyme/config.toml` so this vault's section matches the confirmed config before running init. Do not write `guide.md`; vault-local guide setup is outdated.

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
   For voice agents that need an immediate first turn, use
   `enzyme init --voice-ready --voice-entities 3 --voice-min-catalysts 1`
   instead. It returns once seed petri context exists; the detached worker
   finishes embeddings, final catalysts, and similarities in the background.

7. **Install persistent agent instructions.** `enzyme init --quiet` does not install agent-facing instructions, so app-mediated setup must do it explicitly after init. Use the command for the active agent:

   ```bash
   enzyme install hermes
   # or
   enzyme install openclaw
   # or
   enzyme install codex
   # or
   enzyme install claude
   ```

   These commands fetch the latest instruction templates from GitHub when network access is available, fall back to the packaged copy otherwise, and update only the marked Enzyme section in the workspace context file. Codex, Claude, and Hermes also install the runtime skill into their agent-specific skill directories. Hermes uses the workspace context file, the runtime skill, and plugin hooks together.

8. **Optional retrieval backfill.** If the audit found useful missing structure, propose a separate script and show exactly what it would change before running it. These changes are optional; Enzyme must work on the existing workspace first.
   - Preserve existing frontmatter standards. If notes consistently use `date:`, `created:`, `created_at:`, or another field, continue that standard.
   - Add a date field only when a note lacks frontmatter dates and a date can be inferred from a filename/path regex such as `YYYY-MM-DD`, `YYYYMMDD`, `YYYY/MM/DD`, or from filesystem timestamps. Prefer filename/path dates over filesystem timestamps.
   - Add note-level entity fields only when the entity applies to the whole note and the field already exists in the vault convention. Candidate fields include `people:`, `organizations:`, `companies:`, `clients:`, `projects:`, and `relationships:`.
   - Add `people:` or related entity wikilinks to synced emails, transcripts, meeting notes, or CRM notes by matching existing `people/*.md`, `contacts/*.md`, or client/company pages.
   - Keep frontmatter entity lists selective. Include central people, organizations, clients, companies, projects, tags, and relationships, not every incidental mention.
   - Create missing `people/<Name>.md`, `contacts/<Name>.md`, or company/client stubs only when names are repeatedly referenced, no canonical page exists, and the user confirms the folder naming convention.
   - If an inbox folder is active, propose whether it should stay a retrieval entity, be excluded as transient, or be treated as a capture source whose files graduate into other folders.

   Never backfill frontmatter, create people pages, or move files during first init unless the user explicitly approves the script and scope.

9. **Report setup result.** Tell the user the initialized note count, the persisted entity list, exclusions, what workspace instruction file was updated, and any frontmatter backfill you intentionally left for a separate confirmation. For Hermes, also tell the user to start Hermes from this workspace root and try an orientation prompt such as "What has been active across this workspace lately?"
