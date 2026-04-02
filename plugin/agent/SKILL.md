---
name: enzyme
description: >
  Explore an Obsidian vault using Enzyme — surface connections between ideas,
  find latent patterns across notes. Use when the user wants to explore their
  thinking, draw connections, or search their vault by concept rather than keyword.
license: MIT
compatibility: Requires shell access (macOS arm64, Linux x86_64/arm64). Binary and model are bundled — run scripts/setup.sh if enzyme is not on PATH.
allowed-tools: Bash Read Glob Grep
metadata:
  author: jshph
  version: "0.3.13"
  homepage: https://enzyme.garden
  openclaw:
    requires:
      anyBins: ["enzyme"]
    install:
      - type: download
        url: https://raw.githubusercontent.com/jshph/enzyme/main/install.sh
        run: bash
    os: ["darwin", "linux"]
    always: true
---

# Enzyme — Vault Exploration

Enzyme builds a concept graph from your vault's tags, links, folders, and timestamps. Queries resolve against **catalysts** — pre-computed thematic questions that bridge your content — not raw text. 8ms queries, local embeddings, no runtime LLM calls for search.

## Prerequisites

Enzyme resolves the vault path: `-p` flag > `ENZYME_VAULT_ROOT` env var > current directory.

```bash
ls ${ENZYME_VAULT_ROOT:-.}/.enzyme/enzyme.db 2>/dev/null || ls /tmp/enzyme/enzyme.db 2>/dev/null
```

If `enzyme.db` exists, skip to **Tools**. If not, follow **First-Time Setup**.

## First-Time Setup

### 1. Install enzyme (if not on PATH)

```bash
curl -fsSL https://raw.githubusercontent.com/jshph/enzyme/main/install.sh | bash
```

### 2. Scan the vault

Look at folder structure (by recency and file count), tag frequency (frontmatter and inline, last 90 days), wikilinks (hub pages, reference patterns), and any structural files (CLAUDE.md, AGENTS.md, guide.md). Check file dates: frontmatter `created` field first, filesystem date as fallback.

### 3. Detect existing conventions

Read instruction files (CLAUDE.md, AGENTS.md, guide.md) for capture patterns — where notes get written, tagging habits, organizational structure. Adopt whatever exists. If nothing exists, ask the user once: "Where should I capture session insights?"

### 4. Compose guide and initialize

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

### 5. Write persistent instructions

Add this to AGENTS.md (OpenClaw), CLAUDE.md (Claude Code), or equivalent — filling in the capture folder and conventions from step 3:

```
## Enzyme

On session start, run `enzyme refresh --quiet`.
On the user's first message, run `enzyme petri --query "their message"` for vault context.
Use catalyst phrases from petri to compose `enzyme catalyze` queries.

When a session produces a decision or insight worth preserving,
write a note to [capture folder] using existing vault tags and wikilinks.
```

### 6. Tell the user

> Enzyme indexed N notes across M entities. [Top themes.] Session insights go to [folder].

## Tools

### `enzyme petri` — vault context

```bash
enzyme petri                            # top 10 entities by recency
enzyme petri --query "user's question"  # ranked by relevance
```

Petri is your working memory. Run it on the user's first message to get relevant entities and catalyst vocabulary. Use that vocabulary to compose catalyze queries — catalysts reach content the user's raw words won't find.

Petri + catalyze replace session memory. You don't need to maintain session logs — the vault's full history is already indexed. Notes you write this session get picked up on next refresh, shift entity trends, and surface in future petri calls automatically.

### `enzyme catalyze "query"` — semantic search

```bash
enzyme catalyze "feeling stuck"
enzyme catalyze "what I decided about X" --register continuity
```

**`--register`**: `explore` (default) for patterns, `continuity` for restoring prior decisions, `reference` for capture patterns.

**catalyze vs grep**: use grep for exact anchors (names, tags, wikilinks). Use catalyze when you only have a theme.

### `enzyme refresh` / `enzyme status`

```bash
enzyme refresh --quiet    # incremental, skips if unchanged
enzyme status             # doc count, entity count, coverage
```

### JSON output

**petri**: `name`, `type`, `frequency`, `activity_trend`, `catalysts` (array of `{ text, context }`)

**catalyze**: `results` (array of `{ file_path, content, similarity }`), `top_contributing_catalysts`, `presentation_guidance`

## Writing Notes

Write a note when a session produces a decision, a reframe, or an open thread worth returning to. Don't capture routine Q&A or information already in the vault.

**Filename:** `YYYY-MM-DD-HH-MM-SS topic.md` in the capture folder.

```markdown
---
tags:
  - existing-tag
created: '[[YYYY-MM-DD]]'
---

## descriptive title

The decision or insight in 2-3 sentences. Why it matters.

The tradeoff, the rejected alternative, or what's still open.

Related: [[existing note]]
```

Use existing tags — check petri entities before inventing new ones. Use wikilinks to connect to existing notes. One idea per file so enzyme can track recency per insight. Keep it short.

If a previous decision is superseded, write a new note referencing the old one rather than editing it. Don't reorganize or archive the user's notes — they own vault structure.

## Presentation

Follow [petri-guide.md](references/petri-guide.md) for vault overviews and [search-guide.md](references/search-guide.md) for search results. Lead with the user's own words.

Never expose tool names to the user. Never open with energy assessments of the vault.
