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

Enzyme reads your vault's tags, links, folders, and timestamps and builds a concept graph. Queries resolve against **catalysts** — pre-computed thematic questions that bridge your content — not raw text. 8ms queries, local embeddings, no runtime LLM calls for search.

## Prerequisites

Enzyme resolves the vault path in this order: `-p` flag > `ENZYME_VAULT_ROOT` env var > current directory.

Check if initialized:
```bash
ls ${ENZYME_VAULT_ROOT:-.}/.enzyme/enzyme.db 2>/dev/null || ls /tmp/enzyme/enzyme.db 2>/dev/null
```

If `enzyme.db` exists, skip to **Using Enzyme**. If not, follow **First-Time Setup**.

## First-Time Setup

### 1. Install enzyme (if not on PATH)

```bash
curl -fsSL https://raw.githubusercontent.com/jshph/enzyme/main/install.sh | bash
```

### 2. Scan the vault

Before initializing, understand what's in the vault. Look at:

- **Folder structure**: which folders have markdown, sorted by recency and file count
- **Tag landscape**: frontmatter tags (`- tag` under `tags:`) and inline tags (`#tag`). Count frequency across recent files (last 90 days). Note hierarchical tags (`#travel/pyrenees`).
- **Wikilinks**: frequently referenced notes, hub pages
- **Structural files**: check for CLAUDE.md, AGENTS.md, guide.md, MOC.md, or any file that describes vault conventions
- **File dates**: frontmatter `created` or `date` field first, filesystem created date as fallback

### 3. Detect existing conventions

Read any existing instruction files (CLAUDE.md, AGENTS.md, guide.md). Look for:
- Where notes get written ("capture to inbox/", "daily notes in journal/")
- Tagging habits ("use #project/name for work items")
- Any organizational patterns the user has already established

If conventions exist, adopt them. If none exist, ask the user once: "Where should I capture session insights? I can use `journal/` or match whatever you already do."

### 4. Compose guide and initialize

From the scan, compose a guide — a freeform list of entities to focus on:

```
#research
#design
#people
folder:inbox
folder:projects -- active work

excludedTags:
- todo
- template
- archived
```

Then initialize:
```bash
enzyme init --quiet --guide "<guide content>"
```

The `--quiet` output includes petri data under the `petri` key. Do not run a separate `enzyme petri` call.

### 5. Write persistent instructions

Add the enzyme tools section to the runtime's persistent instruction file (AGENTS.md for OpenClaw, CLAUDE.md for Claude Code, or equivalent). Include:

- The tools available (petri, catalyze, refresh)
- The working memory pattern (run petri on first message, use catalysts to compose catalyze queries)
- Capture conventions discovered or established in step 3

This ensures enzyme tools work on every future session without the full skill being invoked.

### 6. Tell the user what happened

One message:
> Enzyme indexed N notes across M entities. [Brief description of top themes found.]
> Session insights will be captured to [folder] following your existing conventions.

## Using Enzyme

### `enzyme petri` — See what's in the vault

Returns trending entities with catalysts (thematic phrases) and temporal metadata.

```bash
enzyme petri                          # Top 10 entities by recency
enzyme petri -n 5                     # Top 5
enzyme petri --query "user's question" # Entities ranked by relevance
```

With `--query`, results are narrowed to entities and catalysts relevant to the question. Without it, you get the full landscape.

### `enzyme catalyze "query"` — Search by concept

```bash
enzyme catalyze "feeling stuck"
enzyme catalyze "tension between efficiency and presence" -n 20
enzyme catalyze "what I decided about X" --register continuity
```

Compose queries using catalyst vocabulary from petri rather than the user's raw words. Catalysts reach content that generic terms won't find.

**`--register`** controls presentation:
- `explore` (default) — wonder, probe, notice patterns
- `continuity` — restore context, show trajectory
- `reference` — surface capture patterns, connect imports to the user's thinking

### `enzyme refresh` — Update the index

```bash
enzyme refresh --quiet    # Fast: skips if nothing changed
enzyme refresh --full     # Force complete re-index
```

Runs automatically on session start. Manual use is rarely needed.

### `enzyme status` — Vault stats

```bash
enzyme status
```

Returns doc count, entity count, catalyst count, embedding coverage.

### When to use `catalyze` vs `grep`

**Use grep** when you have a concrete anchor — something that exists verbatim:
- Names, tags, wikilinks, file paths, proper nouns

**Use catalyze** when you only have a theme — no exact text to match:
- "What have I written about feeling stuck?"
- "tension between efficiency and presence"

The test: would these exact words appear in their notes? Names and tags always do. Abstract language rarely does.

### Reading JSON output

Both `petri` and `catalyze` return JSON. Read output directly — do not pipe through Python or jq.

**petri** — each entity has: `name`, `type`, `frequency`, `activity_trend`, `days_since_last_seen`, `catalysts` (array of `{ text, context }`)

**catalyze** — response has: `results` (array of `{ file_path, content, similarity }`), `top_contributing_catalysts` (array of `{ text, entity, contribution_count }`), `register`, `presentation_guidance`

## Capture Conventions

When writing notes back to the vault:

- **Use existing tags.** Check petri entities before creating new ones. If `#design` exists, don't create `#design-thinking`.
- **Use wikilinks** to reference existing notes when relevant. `[[open questions]]` creates a connection enzyme can follow.
- **One insight per note** with descriptive filenames, not timestamps alone. `journal/2026-03-28-auth-decision.md` over a bullet in `journal/2026-03-28.md`.
- **Keep capture notes short** — 3-10 lines. The decision, the reasoning, the open thread.
- **Frontmatter tags** for entity association. Enzyme picks up both frontmatter `tags:` and inline `#tags`.

These defaults should be adapted to match whatever conventions already exist in the vault.

## Presentation

When presenting results to the user, follow [petri-guide.md](references/petri-guide.md) for vault overviews and [search-guide.md](references/search-guide.md) for search results. Lead with the user's own words, not metadata.

Never expose tool names (catalyze, petri) to the user. Never open with energy assessments of the vault. The gift is in what you noticed, not in how quickly you connected the dots.
