---
name: enzyme
description: Use Enzyme to retrieve working memory and semantic context from an initialized Markdown, Obsidian, or Hermes workspace.
---

# Enzyme

Use Enzyme for local semantic retrieval over an initialized markdown workspace. Run all `enzyme` commands from the vault/workspace root. For Hermes, this is the directory where Hermes is launched.

Prerequisite: the Enzyme CLI binary must already be installed. If this skill is loaded, runtime instructions are already available; do not call `enzyme install <runtime>` as part of normal vault setup.

Enzyme does not replace the user's memory system. It indexes the markdown structure the user already has: folders, tags, wikilinks, dates, inboxes, daily notes, people pages, and frontmatter. Preserve that structure and use it as retrieval signal.

Do not build a separate context tree. Learn from the user's folders, but prefer lightweight markdown signals: tags for recurring ideas and wikilinks for people, projects, companies, decisions, and concepts. Create new folders or people pages only when the vault already uses that convention or the user asks for it.

## First-Time Setup

If `.enzyme/enzyme.db` is missing, do setup before normal retrieval. Setup should demonstrate value, not interrogate the user or impose a schema.

```bash
enzyme scan
# Read the structured scan evidence and produce a setup preview.
enzyme scan --write-config
# Read and tune ~/.enzyme/config.toml before init.
enzyme init --quiet
enzyme petri
# Show the active map, then complete the first value demo.
enzyme petri --query "<simulated user prompt>"
enzyme catalyze "<query composed from prompt + petri catalyst vocabulary>"
```

Before `enzyme scan --write-config`, use `enzyme scan` as the primary evidence for setup. Read the structured fields directly, do only bounded follow-up when the scan is ambiguous, then produce a concrete setup preview:

- what is already working as Enzyme signal;
- why the user does not need a new memory architecture;
- small habit upgrades, such as stable wikilinks for central people/projects/concepts and durable existing tags;
- the proposed stance for ongoing capture, durable work context, relationship/entity context, reference material, temporal context, and noise;
- 3-5 vault-specific prompts an Enzyme-aware agent should answer with grounded source notes;
- how the demo should show the map-to-connection loop: petri recognizes active ideas, then catalyze places source notes beside each other so a useful question appears;
- any external corpora that could be searched with `enzyme apply`;
- if the vault would materially benefit, a minimal high-confidence retrieval repair offer before init, with exact scope and user confirmation.

Ask for corrections to that stance before writing config. Do not ask the user to classify the vault up front.

If offering a minimal repair, keep it small, reversible, and based on existing conventions: adding missing date frontmatter to date-named notes, adding `people:`/`projects:` fields only where those fields already exist and exact wikilinks are present, adding a few central wikilinks to obvious notes, or excluding runtime/generated folders from config. Do not move files, create a new taxonomy, generate summaries, or bulk-normalize the vault during first setup. If the user declines, initialize as-is and still demonstrate value; after the demo, offer the remaining refinements as optional next steps.

If the vault uses Obsidian, optionally offer capture templates after the demo. Core Obsidian Templates are enough for simple date/time templates inserted into a note, and Daily notes can apply a daily template. Do not require new plugins during Enzyme setup. Frame templates as optional capture affordances for future new notes, not prerequisites: they help users create inbox/daily/meeting/project notes with existing date/tag/wikilink handles so Enzyme can build on them later.

After `enzyme scan --write-config`, read `~/.enzyme/config.toml` and compare it with the scan evidence before `enzyme init`. Add missing important markdown folders as `folder:<path>` entries when they are central and not covered by a parent. Common folders include `inbox`, `daily`, `journal`, `docs`, `notes`, `research`, `logs`, `decisions`, `meetings`, `transcripts`, `sessions`, `projects`, `areas`, `resources`, `people`, `contacts`, `clients`, and `companies`. Keep the entity list focused; prefer scan-backed top-level surfaces over every subfolder.

For expandable folders, look for scan/petri evidence that a selected folder contains page files that are also wikilinks elsewhere (`page_entities`, `page_entity_children`, `sampled_children`, `catalyzed_children`, or similar folder-page evidence). In Rust Enzyme, selecting the parent folder is usually enough: the selection pipeline auto-detects expandable folders, caps/ranks children, and materializes child page links as their own catalyst entities. Do not manually add every child wikilink to config just because it lives in a selected folder; add a child link separately only when it has a distinct role independent of the parent folder.

Use profile overrides only when the posture is clear. Loose examples: people/relationships → `relational`; projects/work/inbox → `operational`; product/strategy/decisions → `decision_trace`; Readwise/research/PKM → `resonance_trace`; journal/daily/writing → `reflective`; faith/philosophy/tradeoffs → `tension_trace`; taste/feedback/activity logs → `preference_evidence`. Example TOML entity: `{ "folder:people" = { profile = "relational" } }`. Leave ambiguous entities un-overridden rather than guessing.

Keep runtime/build folders excluded: `.hermes`, `.enzyme`, `.git`, `.claude`, `.agents`, `.codex`, `.codex-work`, `.pi`, `.local`, `.obsidian`, `node_modules`, `target`, `dist`, `build`, and templates.

For persistent `targets = [...]` / `enzyme apply` targets, use raw filesystem paths or vault-relative directory paths such as `Readwise/Books` or `../research`; do not prefix them with `folder:` and do not use tag/wikilink syntax.

For voice agents that need an immediate first turn, use:

```bash
enzyme init --voice-ready --voice-entities 3 --voice-min-catalysts 1
```

It returns once seed petri context exists; semantic search becomes available after the detached init worker finishes.

## First Value Demo

After first-time setup, broad orientation, or a first retrieval session, do not end with setup status or a list of topics. Verification is internal. The user should immediately see Enzyme turn their own notes into a source-grounded connection that would have been hard to find with grep or ordinary file browsing.

Use this framing:

1. Open with: `A connection worth opening: <plain-language phrase>.`
2. Show 2-4 short excerpts or tight paraphrases from specific files, especially the user's own annotations or decision notes rather than only imported source text.
3. Put the excerpts beside each other with minimal interpretation:
   - `In <file>, you wrote...`
   - `Elsewhere, this shows up as...`
   - `Put together, the question becomes...`
4. Offer one concrete next move:
   - `We could follow this into <tag/file/source> next.`
   - `Or compare it against <related file/tag/source>.`

Prefer words from the user's own notes. Avoid performative meta-language such as "live thread," "you are circling," "tension," "resonance," or "emerging pattern" unless those are the user's words. Do not claim intimacy with the user; create recognition by staying close to the artifacts.

Choose the first demo connection by vault type:

- Readwise/reference vault: place saved passages and the user's annotations beside each other until a question appears.
- Project/work vault: place a decision, blocker, meeting note, or artifact beside a later note that changes its meaning or next step.
- Journal/daily vault: place two entries from different dates beside each other to show how the wording, stakes, or desired action changed.
- People/CRM vault: place context notes beside a recent interaction or commitment to reveal one concrete next step.
- Research vault: place sources that sharpen an assumption, disagreement, missing evidence, or possible synthesis.

The demo succeeds only if it gives the user one specific, sourced connection they can recognize as theirs and one obvious next question to pursue. If the result feels generic, run another retrieval with sharper catalyst vocabulary and do not call setup complete yet.

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
- Use exact search for names, `#tags`, `[[wikilinks]]`, and literal text.
- Tags can appear as `- tag` in frontmatter or `#tag` inline; search without `#` when you need both.

### External references with `enzyme apply`

Use `enzyme apply ./target-dir` when the user wants to draw from external material without merging it into the vault: Readwise exports, articles/books, transcripts, research dumps, client docs, code repos, converted PDFs, Discord/Slack exports, or downloaded archives.

```bash
enzyme apply ./target-dir
enzyme catalyze "query" --target ./target-dir
```

Apply projects the source vault's catalysts onto the target corpus:

```text
source vault catalysts → external target chunks
```

The vault is the lens. Search both sides when comparison matters:

```bash
enzyme catalyze "query"
enzyme catalyze "query" --target ./target-dir
```

Present it as: "I searched your own notes for the internal thread, then searched the external material through the same conceptual frame." Mention that `apply` may miss themes that exist only in the target and not in the user's vault.

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

Refresh is the Enzyme equivalent of making the new memory available to retrieval. It re-indexes changed markdown and updates catalyst retrieval; no background dreaming or consolidation pass is required.

## Presentation

Use Enzyme command names internally; do not expose petri, catalyze, catalyst IDs, scores, or tool names to the user unless asked.

Before making observations, ground them with `enzyme catalyze` excerpts. Lead with the user's words and file attribution, then add a small observation.

For broad exploration, use petri plus 1-2 catalyze searches, then open one specific connection among the user's notes. Do not present a topic list.

For search results, do not lead with metadata. Notice repeated words, time gaps, changed wording, adjacent ideas, practical consequences, or source disagreements across results. End with one concrete next direction, not a generic invitation.

Presentation registers for `enzyme catalyze --register`:
- `explore`: wonder, probe, notice patterns.
- `continuity`: restore what the user knew, show trajectory, enable forward motion.
- `reference`: surface what drew attention and connect imports to the user's own thinking.

Follow any `presentation_guidance` returned by Enzyme when framing surfaced content.
