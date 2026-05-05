# Enzyme — First-Time Setup

Run this workflow when `enzyme.db` doesn't exist in the vault. After setup, this guide is no longer needed — the SKILL.md and persistent instructions handle everything.

## 1. Install enzyme (if not on PATH)

```bash
curl -fsSL https://raw.githubusercontent.com/jshph/enzyme/main/install.sh | bash
```

## 2. Scan the vault

Run `enzyme scan` first. This is a preview only; do not run `enzyme scan --write-config` until the user has confirmed what should be persisted.

Then audit the vault independently: folder structure (by recency and file count), tag frequency (frontmatter and inline, last 90 days), wikilinks (hub pages, reference patterns), and any structural files (CLAUDE.md, AGENTS.md, README.md, MOC/Index files). Check file dates: frontmatter `created`/`date` field first, filename/path dates second, filesystem date as fallback.

If there is no API key in the environment and no `~/.enzyme/auth.json`, run `enzyme login --json --no-open`. Read JSONL events directly: `already_logged_in` means continue; `device_authorization` means show the `verification_uri` to the user and keep the command running; `success` means continue; `expired` or `error` means explain the failure and retry only if the user asks. Do not ask the user to paste API keys.

## 3. Detect existing conventions

Read instruction files (CLAUDE.md, AGENTS.md, README.md) for capture patterns — where notes get written, tagging habits, organizational structure, and frontmatter conventions. Sample notes to see whether the vault uses `created: '[[YYYY-MM-DD]]'`, `people:`, topical `tags:`, or other fields. Adopt whatever exists. If nothing exists, ask the user once: "Where should I capture session insights?"

## 4. Compare, confirm, and persist config

Show the user:

- What `enzyme scan` proposed
- What your independent audit confirmed
- What scan missed
- What scan suggested that looks structural, stale, accidental, or risky
- The exact entities and exclusions you plan to persist

Use the global config as the persisted source of truth. Do not write `guide.md`; vault-local guide setup is outdated.

After user confirmation, run:

```bash
enzyme scan --write-config
```

If the user corrected the proposed list, edit `~/.enzyme/config.toml` so this vault's section matches the confirmed config:

```toml
[vaults."/path/to/vault"]
entities = ["#research", "#design", "folder:inbox", "folder:projects"]
excluded_tags = ["todo", "template"]
excluded_folders = [".claude", ".enzyme", ".git", ".obsidian", "templates"]
```

```bash
enzyme init --quiet
```

The `--quiet` output includes petri data under the `petri` key — don't run a separate petri call.

## 5. Optional frontmatter backfill

If the audit found useful missing structure, propose a script and show exactly what it would change before running it. Good candidates:

- Add `created: '[[YYYY-MM-DD]]'` when a file has a date in its filename/path but no frontmatter date
- Add `people:` wikilinks to emails/transcripts by matching existing `people/*.md` notes
- Create missing `people/<Name>.md` stubs only when names are repeatedly referenced and the user confirms

Never backfill frontmatter during first init unless the user explicitly approves the script and scope.

## 6. Write persistent instructions

Add this to AGENTS.md (OpenClaw), CLAUDE.md (Claude Code), or equivalent — filling in the capture folder and conventions from step 3:

```
## Enzyme

On session start, run `enzyme refresh --quiet`.
On the user's first message, run `enzyme petri --query "their message"` for vault context.
Use catalyst phrases from petri to compose `enzyme catalyze` queries.

When a session produces a decision or insight worth preserving,
write a note to [capture folder] using existing vault tags and wikilinks.
```

## 7. Tell the user

> Enzyme indexed N notes across M entities. [Top themes.] Session insights go to [folder].
