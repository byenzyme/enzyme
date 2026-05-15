---
name: enzyme
description: >
  Explore an Obsidian, Markdown, or Hermes agent workspace using Enzyme —
  surface connections between ideas, find latent patterns across notes. Use
  when the user wants to explore their thinking, draw connections, or search
  their workspace by concept rather than keyword.
license: MIT
compatibility: Requires shell access (macOS arm64/x86_64, Linux x86_64/arm64). Install the enzyme CLI if it is not on PATH.
allowed-tools: Bash Read Glob Grep
metadata: { "openclaw": { "always": true, "os": ["darwin", "linux"], "primaryEnv": "OPENROUTER_API_KEY", "requires": { "anyBins": ["enzyme"] }, "install": [{ "id": "curl", "kind": "download", "url": "https://raw.githubusercontent.com/jshph/enzyme/main/install.sh", "bins": ["enzyme"], "label": "Install enzyme (curl)" }] }, "author": "jshph", "version": "0.5.10", "homepage": "https://enzyme.garden" }
---

# Enzyme

Enzyme builds a concept graph from your vault's tags, links, folders, and timestamps. Queries resolve against **catalysts** — pre-computed thematic questions that bridge content. 8ms queries, local embeddings.

Vault path: `-p` flag > `ENZYME_VAULT_ROOT` env var > current directory.

For Hermes, this skill is for operational use inside a user's workspace, not for developing Hermes itself. Run Enzyme from the directory where Hermes is launched so the same `AGENTS.md`/`.hermes.md` context and markdown corpus are visible to both.

Respect the user's existing markdown system. Enzyme should read Obsidian-style folders, inboxes, daily notes, people pages, tags, wikilinks, and frontmatter conventions before suggesting any structure. Do not impose a memory schema or context tree; use existing structure as retrieval signal and propose optional backfills only when the user asks for setup help. Prefer tags for recurring ideas and wikilinks for people, projects, companies, decisions, and concepts before creating folders or person-specific structures. Preserve existing note-level entity fields such as `people:`, `organizations:`, `companies:`, `clients:`, `projects:`, and `relationships:`.

If the vault isn't initialized (no `.enzyme/enzyme.db`), run first-time setup from the vault root:

```bash
enzyme scan
# Review proposed entities/exclusions against the workspace.
enzyme scan --write-config
enzyme init --quiet
enzyme install hermes
# or: enzyme install openclaw
# or: enzyme install codex
# or: enzyme install claude
```

Use `enzyme install hermes`, `enzyme install openclaw`, `enzyme install codex`, or `enzyme install claude` for the active runtime. The install command fetches current instruction templates from GitHub when available and falls back to the packaged copy.

Before `enzyme init`, read `~/.enzyme/config.toml` after `enzyme scan --write-config` and compare it with the workspace scan. Add missing important markdown folders as `folder:<path>` entries when they are central to the workspace and not covered by a parent. Common folders include `inbox`, `daily`, `journal`, `docs`, `notes`, `research`, `logs`, `decisions`, `meetings`, `transcripts`, `sessions`, `projects`, `areas`, `resources`, `people`, `contacts`, `clients`, and `companies`. Keep runtime/build folders excluded: `.hermes`, `.enzyme`, `.git`, `.claude`, `.obsidian`, `node_modules`, `target`, `dist`, `build`, and templates.

For voice agents that need to start the first turn before the full init barrier,
use `enzyme init --voice-ready --voice-entities 3 --voice-min-catalysts 1`.
It returns once seed petri context exists; semantic search becomes available
after the detached init worker finishes.

## Session Lifecycle

What's automatic depends on your runtime:

**Hermes** (hooks handle it):
- **First install** — the plugin can bootstrap the binary; the workspace still needs `enzyme scan`, TOML validation, `enzyme init`, and `enzyme install hermes` once
- **Session start** — binary bootstrap + `enzyme refresh` run automatically
- **Each turn** — `enzyme petri --query` injects vault context before the model sees your message
- **Session end** — after any useful markdown notes are written, `enzyme refresh` indexes them

**OpenClaw** (skill instructions + config):
- **Session start** — run `enzyme refresh --quiet` (add to AGENTS.md or heartbeat)
- **First turn / context-dependent turns** — run `enzyme petri --query "user's message"` before responding, unless the plugin already injected petri context
- **Session end** — write useful markdown notes if the session produced durable memory, then run `enzyme refresh --quiet`
- **Between sessions** — heartbeat or cron can keep the index fresh for external syncs (see README for config)

In both cases: you call petri and catalyze as tools. The difference is whether context injection is automatic (Hermes hooks) or agent-driven (OpenClaw skill instructions).

## Tools

### `enzyme petri` — vault context

```bash
enzyme petri                            # entities by recency
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

### `enzyme apply ./target-dir` — project catalysts onto other content

```bash
enzyme apply ./target-dir
enzyme catalyze "query" --target ./target-dir
```

Apply indexes external content using the vault's catalysts. Use it when the user wants to bring the shape of their existing knowledge to another folder or corpus.

### `enzyme refresh` / `enzyme status`

```bash
enzyme refresh --quiet    # incremental, skips if unchanged
enzyme status             # doc count, entity count, coverage
```

### JSON output

**petri**: `name`, `type`, `frequency`, `activity_trend`, `catalysts` (array of `{ text, context }`)

**catalyze**: `results` (array of `{ file_path, content, similarity }`), `top_contributing_catalysts`, `presentation_guidance`

## Writing Notes

Write memory as ordinary markdown, not as a separate memory store. The point is to leave useful notes that Enzyme can refresh and retrieve through Petri, Catalyze, and Apply.

The best time to write is near the end of a session or after a meaningful decision, when the durable outcome is clear. Don't interrupt the user's flow to capture routine Q&A.

Write a note when a session produces a decision, a reframe, an open thread worth returning to, a durable preference, a project state change, or useful people/company context.

Do not write a note for raw tool output, one-time commands, transient status, generic summaries, or facts already captured without material change. Never store secrets, credentials, tokens, or raw config values; if relevant, record only that a credential was configured.

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

Before writing, use `enzyme petri`, `enzyme catalyze`, or exact search to find related notes. Link to existing notes when possible.

Use existing tags — check petri entities before inventing new ones. Use wikilinks to connect to existing notes. One idea per file. Keep it short.

When writing into an existing Obsidian vault, follow its current folder and frontmatter conventions. If the vault has people/contact pages, link them instead of creating parallel names. If no people/company folder exists, use wikilinks and existing tags before proposing a new structure. If no convention exists, ask before introducing a capture folder, date field, people folder, or context-tree-like structure.

For entities that apply to the whole note, prefer existing frontmatter fields over repeating names throughout the body. Include only central people, organizations, clients, companies, projects, tags, or relationships from the retrieved context. Use inline wikilinks where a mention matters locally in the prose. Do not add new entity fields unless the vault already uses them or the user approves the convention.

If a previous decision is superseded, write a new note referencing the old one rather than editing it.

After writing memory notes at the end of the session, run:

```bash
enzyme refresh --quiet
```

Refresh is the Enzyme equivalent of making the new memory live. It re-indexes changed markdown and updates catalyst retrieval; no background dreaming or consolidation pass is required.

## Presentation

Lead with the user's own words from matched excerpts. For vault overviews, name something the user is doing across the vault, then ground it with concrete source breadcrumbs. For search results, use any `presentation_guidance` returned in JSON, notice tensions across results, and suggest specific next searches using catalyst language.

Never expose tool names to the user. Never open with energy assessments of the vault.
