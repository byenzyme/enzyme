---
name: enzyme
description: Use Enzyme to retrieve working memory and semantic context from an initialized Markdown, Obsidian, or Hermes workspace.
---

# Enzyme

Use Enzyme for local semantic retrieval over an initialized markdown workspace. Run all `enzyme` commands from the vault/workspace root. For Hermes, this is the directory where Hermes is launched.

Enzyme does not replace the user's memory system. It indexes the markdown structure the user already has: folders, tags, wikilinks, dates, inboxes, daily notes, people pages, and frontmatter. Preserve that structure and use it as retrieval signal.

Do not build a separate context tree. Learn from the user's folders, but prefer lightweight markdown signals: tags for recurring ideas and wikilinks for people, projects, companies, decisions, and concepts. Create new folders or people pages only when the vault already uses that convention or the user asks for it.

## First-Time Setup

If `.enzyme/enzyme.db` is missing, do setup before normal retrieval:

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

Use `enzyme install hermes`, `enzyme install openclaw`, `enzyme install codex`, or `enzyme install claude` for the active runtime.

Before `enzyme init`, read `~/.enzyme/config.toml` and compare it with the workspace. Add missing important markdown folders as `folder:<path>` entries when they are central and not covered by a parent. Common folders include `inbox`, `daily`, `journal`, `docs`, `notes`, `research`, `logs`, `decisions`, `meetings`, `transcripts`, `sessions`, `projects`, `areas`, `resources`, `people`, `contacts`, `clients`, and `companies`.

Keep runtime/build folders excluded: `.hermes`, `.enzyme`, `.git`, `.claude`, `.obsidian`, `node_modules`, `target`, `dist`, `build`, and templates.

For voice agents that need an immediate first turn, use:

```bash
enzyme init --voice-ready --voice-entities 3 --voice-min-catalysts 1
```

It returns once seed petri context exists; semantic search becomes available after the detached init worker finishes.

## Existing Structure

Do not impose a new memory schema.

- Follow existing Obsidian or markdown conventions before suggesting changes.
- Treat inboxes, daily notes, project folders, CRM folders, tags, wikilinks, and frontmatter as signal.
- If a `people/`, `contacts/`, `clients/`, or `companies/` folder exists, treat it as canonical for person/company references.
- If no people/company folder exists, prefer wikilinks and existing tags over creating a new per-person knowledge tree.
- Preserve existing date field names such as `date:`, `created:`, or `created_at:` when they are consistent.
- Preserve existing entity fields such as `people:`, `organizations:`, `companies:`, `clients:`, `projects:`, or `relationships:` when the vault uses them.
- Propose frontmatter dates, people-page creation, or folder changes only when the user is explicitly doing setup or asks for structure improvement.

Optional backfills must be reviewed before running. Good candidates are date frontmatter inferred from filenames/paths, note-level entity fields matched to existing wikilinks, and repeated person/company names that the user confirms should become CRM pages.

## Working Memory

`enzyme petri` returns current entities and catalysts, which are thematic phrases from the vault.

- For a specific user prompt, run `enzyme petri --query "user's question"`.
- For a broad prompt or first orientation, run `enzyme petri`.
- Treat nested children under a tag or folder as evidence inside that parent cluster by default.

Use catalyst phrases as vocabulary for `enzyme catalyze` searches. They connect to precomputed content that the user's raw words may not find.

## Search

- `enzyme catalyze "query"` searches by concept/theme. Compose queries from petri catalyst vocabulary.
- `enzyme refresh --quiet` re-indexes changed content.
- `enzyme apply ./target-dir` indexes external content using vault catalysts; then search it with `enzyme catalyze "query" --target ./target-dir`.
- Use exact search for names, `#tags`, `[[wikilinks]]`, and literal text.
- Tags can appear as `- tag` in frontmatter or `#tag` inline; search without `#` when you need both.

## Writing Notes

Write memory as ordinary markdown, not as a separate memory store. The point is to leave useful notes that Enzyme can refresh and retrieve through Petri, Catalyze, and Apply.

The best time to write is near the end of a session or after a meaningful decision, when the durable outcome is clear. Do not interrupt the user's flow to capture routine Q&A.

Write a note when a session produces a decision, a reframe, an open thread worth returning to, a durable preference, a project state change, or useful people/company context.

Do not write a note for raw tool output, one-time commands, transient status, generic summaries, or facts already captured without material change. Never store secrets, credentials, tokens, or raw config values; if relevant, record only that a credential was configured.

Follow the vault's existing folder and frontmatter conventions. If no convention exists, ask before introducing a capture folder, date field, people folder, or context-tree-like structure.

Before writing, use `enzyme petri`, `enzyme catalyze`, or exact search to find related notes. Link to existing notes when possible. If a previous decision is superseded, write a new dated note referencing the old one rather than editing history in place.

Use existing tags and wikilinks. Check petri entities before inventing new tags. Use wikilinks for people and ideas when they help future retrieval; do not create standalone person pages unless the vault already has that pattern or the user confirms it. Preserve the user's exact wording for preferences, opinions, and stated rules when that wording matters.

For entities that apply to the whole note, prefer existing frontmatter fields over repeating the same names throughout the body. Examples include `people:`, `organizations:`, `companies:`, `clients:`, `projects:`, and `relationships:`. Only add fields already used by the vault or explicitly approved by the user. Keep entity lists selective: include the people, organizations, clients, companies, tags, and relationships that are central to the note, not every incidental mention from retrieved context.

If the vault has no stronger template, write compact notes in this shape:

```markdown
---
tags:
  - existing-tag
created: '[[YYYY-MM-DD]]'
---

## descriptive title

The decision, reframe, or open thread in 2-3 sentences. Why it matters.

Related: [[existing note]]
```

Omit empty optional fields unless the vault commonly keeps them.

After writing memory notes at the end of the session, run:

```bash
enzyme refresh --quiet
```

Refresh is the Enzyme equivalent of making the new memory live. It re-indexes changed markdown and updates catalyst retrieval; no background dreaming or consolidation pass is required.

## Presentation

Use Enzyme command names internally; do not expose petri, catalyze, catalyst IDs, scores, or tool names to the user unless asked.

Before making observations, ground them with `enzyme catalyze` excerpts. Lead with the user's words and file attribution, then add a small observation.

For broad exploration, use petri plus 1-2 catalyze searches, then open with one specific question about what the user is doing across their notes. Do not present a topic list.

For search results, do not lead with metadata. Notice tensions, repeated words, time gaps, or changes in framing across results. End with one concrete next direction, not a generic invitation.

Presentation registers for `enzyme catalyze --register`:
- `explore`: wonder, probe, notice patterns.
- `continuity`: restore what the user knew, show trajectory, enable forward motion.
- `reference`: surface what drew attention and connect imports to the user's own thinking.

Follow any `presentation_guidance` returned by Enzyme when framing surfaced content.
