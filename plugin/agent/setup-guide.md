# Enzyme — First-Time Setup

Run this workflow when `enzyme.db` doesn't exist in the vault. After setup, this guide is no longer needed — the SKILL.md and persistent instructions handle everything.

## 1. Install enzyme (if not on PATH)

```bash
curl -fsSL https://raw.githubusercontent.com/jshph/enzyme/main/install.sh | bash
```

## 2. Scan the vault

Look at folder structure (by recency and file count), tag frequency (frontmatter and inline, last 90 days), wikilinks (hub pages, reference patterns), and any structural files (CLAUDE.md, AGENTS.md, guide.md). Check file dates: frontmatter `created` field first, filesystem date as fallback.

## 3. Detect existing conventions

Read instruction files (CLAUDE.md, AGENTS.md, guide.md) for capture patterns — where notes get written, tagging habits, organizational structure. Adopt whatever exists. If nothing exists, ask the user once: "Where should I capture session insights?"

## 4. Compose guide and initialize

From the scan, compose a freeform guide — entity names, `folder:` prefixes, `excludedTags:` block:

```
#research
#design
folder:inbox
folder:projects -- active work

excludedTags:
- todo
- template
```

```bash
enzyme init --quiet --guide "<guide content>"
```

The `--quiet` output includes petri data under the `petri` key — don't run a separate petri call.

## 5. Write persistent instructions

Add this to AGENTS.md (OpenClaw), CLAUDE.md (Claude Code), or equivalent — filling in the capture folder and conventions from step 3:

```
## Enzyme

On session start, run `enzyme refresh --quiet`.
On the user's first message, run `enzyme petri --query "their message"` for vault context.
Use catalyst phrases from petri to compose `enzyme catalyze` queries.

When a session produces a decision or insight worth preserving,
write a note to [capture folder] using existing vault tags and wikilinks.
```

## 6. Tell the user

> Enzyme indexed N notes across M entities. [Top themes.] Session insights go to [folder].
