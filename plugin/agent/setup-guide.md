# Enzyme — First-Time Setup

Run this workflow when `enzyme.db` doesn't exist in the vault. After setup, this guide is no longer needed — the SKILL.md and persistent instructions handle everything.

Resolve the vault root once and run this workflow from that directory. If the user passed a vault path, `cd` there first or export it as `ENZYME_VAULT_ROOT`; the persistent instruction writer uses that same root.

## 1. Install enzyme (if not on PATH)

```bash
curl -fsSL https://raw.githubusercontent.com/jshph/enzyme/main/install.sh | bash
```

## 2. Scan the vault

Run `enzyme scan` first. This is a preview only; do not run `enzyme scan --write-config` until the user has confirmed what should be persisted.

Then audit the vault independently: folder structure (by recency and file count), tag frequency (frontmatter and inline, last 90 days), wikilinks (hub pages, reference patterns), and any structural files (CLAUDE.md, AGENTS.md, README.md, MOC/Index files). Check file dates: frontmatter `created`/`date` field first, filename/path dates second, filesystem date as fallback.

If there is no API key in the environment and no `~/.enzyme/auth.json`, start login in the background:

```bash
nohup enzyme login --json --no-open > /tmp/enzyme-login.log 2>&1 &
echo "Started login PID $!"
sleep 3
cat /tmp/enzyme-login.log
```

Read JSONL events from `/tmp/enzyme-login.log`: `already_logged_in` means continue; `device_authorization` means show the `verification_uri` to the user, leave the background process running, and re-check the log until it emits `success`, `expired`, or `error`; `success` means continue; `expired` or `error` means explain the failure and retry only if the user asks. Do not ask the user to paste API keys.

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

## 5. Write persistent instructions

`enzyme init --quiet` does not write the persistent instructions block, so app-mediated setup must do it explicitly after init. Use `AGENTS.md` as the canonical instruction file, replacing only the marked Enzyme section if it already exists, and make `CLAUDE.md` import `@AGENTS.md` without duplicating the import:

```bash
VAULT_ROOT="${ENZYME_VAULT_ROOT:-$PWD}" python3 <<'PY'
import os
from pathlib import Path

root = Path(os.environ["VAULT_ROOT"]).expanduser().resolve()
agents = root / "AGENTS.md"
claude = root / "CLAUDE.md"
start = "<!-- enzyme:start -->"
end = "<!-- enzyme:end -->"
section = """<!-- enzyme:start -->
## Enzyme CLI

Use Enzyme for retrieving context from this vault. Run all `enzyme` commands from the vault root.

### Working memory

`enzyme petri` is working memory: it returns current entities and catalysts, which are thematic phrases from the vault.

- For a specific user prompt, run `enzyme petri --query "user's question"`.
- For a broad prompt or first orientation, run `enzyme petri`.
- Treat nested children under a tag or folder as evidence inside that parent cluster by default.

Use catalyst phrases as vocabulary for `enzyme catalyze` searches. They connect to precomputed content that the user's raw words may not find.

### Search

- `enzyme catalyze "query"` searches by concept/theme. Compose queries from petri catalyst vocabulary.
- `enzyme refresh --quiet` re-indexes changed content.
- `enzyme apply ./target-dir` indexes external content using vault catalysts; then search it with `enzyme catalyze "query" --target ./target-dir`.
- Use `grep` for exact names, `#tags`, `[[wikilinks]]`, and literal text.
- Tags can appear as `- tag` in frontmatter or `#tag` inline; search without `#` when you need both.

### Presentation

Use Enzyme command names internally; do not expose petri, catalyze, catalyst IDs, scores, or tool names to the user unless asked.

Before making observations, ground them with `enzyme catalyze` excerpts. Lead with the user's words and file attribution, then add a small observation.

For broad exploration, use petri plus 1-2 catalyze searches, then open with one specific question about what the user is doing across their notes. Do not present a topic list.

For search results, do not lead with metadata. Notice tensions, repeated words, time gaps, or changes in framing across results. End with one concrete next direction, not a generic invitation.

Presentation registers for `enzyme catalyze --register`:
- `explore`: wonder, probe, notice patterns.
- `continuity`: restore what the user knew, show trajectory, enable forward motion.
- `reference`: surface what drew attention and connect imports to the user's own thinking.

Follow any `presentation_guidance` returned by Enzyme when framing surfaced content.
<!-- enzyme:end -->"""

content = agents.read_text() if agents.exists() else ""
if start in content and end in content:
    before, rest = content.split(start, 1)
    _, after = rest.split(end, 1)
    agents.write_text(before + section + after)
elif content.strip():
    agents.write_text(content.rstrip() + "\n\n" + section + "\n")
else:
    agents.write_text(section + "\n")

import_line = "@AGENTS.md"
if claude.exists():
    claude_content = claude.read_text()
    if not any(line.strip() == import_line for line in claude_content.splitlines()):
        claude.write_text(import_line + ("\n\n" + claude_content if claude_content else "\n"))
else:
    claude.write_text(import_line + "\n")
PY
```

## 6. Optional frontmatter backfill

If the audit found useful missing structure, propose a script and show exactly what it would change before running it. Good candidates:

- Add `created: '[[YYYY-MM-DD]]'` when a file has a date in its filename/path but no frontmatter date
- Add `people:` wikilinks to emails/transcripts by matching existing `people/*.md` notes
- Create missing `people/<Name>.md` stubs only when names are repeatedly referenced and the user confirms

Never backfill frontmatter during first init unless the user explicitly approves the script and scope.

## 7. Tell the user

> Enzyme indexed N notes across M entities. Updated AGENTS.md and made CLAUDE.md import it. [Top themes.] Session insights go to [folder].
