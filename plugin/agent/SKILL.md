---
name: enzyme
description: >
  Explore an Obsidian vault using Enzyme — surface connections between ideas,
  find latent patterns across notes. Use when the user wants to explore their
  thinking, draw connections, or search their vault by concept rather than keyword.
license: MIT
compatibility: Requires shell access (macOS arm64, Linux x86_64/arm64). Binary and model are bundled — run scripts/setup.sh if enzyme is not on PATH.
allowed-tools: Bash Read Glob Grep
metadata: { "openclaw": { "always": true, "os": ["darwin", "linux"], "primaryEnv": "OPENROUTER_API_KEY", "requires": { "anyBins": ["enzyme"] }, "install": [{ "id": "curl", "kind": "download", "url": "https://raw.githubusercontent.com/jshph/enzyme/main/install.sh", "bins": ["enzyme"], "label": "Install enzyme (curl)" }] }, "author": "jshph", "version": "0.4.5", "homepage": "https://enzyme.garden" }
---

# Enzyme

Enzyme builds a concept graph from your vault's tags, links, folders, and timestamps. Queries resolve against **catalysts** — pre-computed thematic questions that bridge content. 8ms queries, local embeddings.

Vault path: `-p` flag > `ENZYME_VAULT_ROOT` env var > current directory.

If the vault isn't initialized (no `.enzyme/enzyme.db`), follow [setup-guide.md](setup-guide.md) first.

## Session Lifecycle

What's automatic depends on your runtime:

**Hermes** (hooks handle it):
- **Session start** — binary bootstrap + `enzyme refresh` run automatically
- **Each turn** — `enzyme petri --query` injects vault context before the model sees your message
- **Session end** — index refreshes to pick up any notes written during the session

**OpenClaw** (skill instructions + config):
- **Session start** — run `enzyme refresh --quiet` (add to AGENTS.md or heartbeat)
- **Each turn** — run `enzyme petri --query "user's message"` before responding
- **Between sessions** — heartbeat or cron keeps the index fresh (see README for config)

In both cases: you call petri and catalyze as tools. The difference is whether context injection is automatic (Hermes hooks) or agent-driven (OpenClaw skill instructions).

## Tools

### `enzyme petri` — vault context

```bash
enzyme petri                            # top 10 entities by recency
enzyme petri --query "user's question"  # ranked by relevance
```

Petri is your working memory. Run it on the user's first message to get relevant entities and catalyst vocabulary. Compose catalyze queries using that vocabulary — catalysts reach content the user's raw words won't find.

You don't need session logs. The vault's full history is already indexed. Notes you write this session get picked up on next refresh, shift entity trends, and surface in future petri calls automatically.

### `enzyme catalyze "query"` — semantic search

```bash
enzyme catalyze "feeling stuck"
enzyme catalyze "what I decided about X" --register continuity
```

**`--register`**: `explore` (default) for patterns, `continuity` for restoring prior decisions, `reference` for capture patterns.

Use grep for exact anchors (names, tags, wikilinks). Use catalyze when you only have a theme.

### `enzyme refresh` / `enzyme status`

```bash
enzyme refresh --quiet    # incremental, skips if unchanged
enzyme status             # doc count, entity count, coverage
```

### JSON output

**petri**: `name`, `type`, `frequency`, `activity_trend`, `catalysts` (array of `{ text, context }`)

**catalyze**: `results` (array of `{ file_path, content, similarity }`), `top_contributing_catalysts`, `presentation_guidance`

## Writing Notes

Write a note when a session produces a decision, a reframe, or an open thread worth returning to. Don't capture routine Q&A.

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

Use existing tags — check petri entities before inventing new ones. Use wikilinks to connect to existing notes. One idea per file. Keep it short.

If a previous decision is superseded, write a new note referencing the old one rather than editing it.

## Presentation

Follow [petri-guide.md](references/petri-guide.md) for vault overviews and [search-guide.md](references/search-guide.md) for search results. Lead with the user's own words.

Never expose tool names to the user. Never open with energy assessments of the vault.
