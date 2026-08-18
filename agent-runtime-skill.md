---
name: enzyme
description: >
  Use Enzyme in an already activated Obsidian, Markdown, or Hermes workspace
  for working-memory retrieval, source-grounded exploration, semantic search by
  concept rather than keyword, runtime note-writing, and refresh. If the
  workspace is not initialized, follow the first-time setup section before
  normal retrieval.
---

# Enzyme

Use Enzyme for local semantic retrieval over an initialized markdown workspace. Run all `enzyme` commands from the vault/workspace root. For Hermes, this is the directory where Hermes is launched.

Vault path: `-p` flag > `ENZYME_VAULT_ROOT` env var > current directory.

For Hermes, this skill is for operational use inside a user's workspace, not for developing Hermes itself. Run Enzyme from the directory where Hermes is launched so the same `AGENTS.md`/`.hermes.md` context and markdown corpus are visible to both.

Prerequisite: check the installed binary first with `enzyme --version`. If the binary is missing, install Enzyme before setup. If Enzyme is installed but old, tell the user you are upgrading Enzyme to the latest release before setup, then run the normal installer path. If the pre-upgrade version is older than `0.5.8`, warn that older Enzyme releases used a different auth/provider contract; after upgrading, ask before clearing stale authentication. Never delete `~/.enzyme/auth.json` or run `enzyme logout` without explicit confirmation. If this skill is loaded, runtime instructions are already available; do not call `enzyme install <runtime>` as part of normal vault setup.

Auth and provider safety: let Enzyme decide when auth is needed. Do not preflight `~/.enzyme/auth.json` or start login before a command asks for it. If an Enzyme command reports that login is required, start device login in the background, read JSONL events from `/tmp/enzyme-login.log`, show the verification URI/code when present, wait for success/error/expiry, then retry the original command. Never ask the user to paste API keys or auth tokens.

Never print API key values. During setup, before `enzyme init`, inspect only which LLM env variable names exist:

```bash
env | cut -d= -f1 | grep -E '^(OPENROUTER_API_KEY|OPENROUTER_BASE_URL|OPENROUTER_MODEL|OPENAI_API_KEY|OPENAI_BASE_URL|OPENAI_MODEL|ENZYME_LOCAL_MODEL)$'
```

Enzyme ignores inherited LLM env keys by default. Do not unset env vars as a workaround, and do not silently spend the user's personal OpenAI/OpenRouter key just because it exists in the shell. If `OPENAI_API_KEY` is present, warn that using env credentials intentionally should normally include the complete OpenAI-compatible triple: `OPENAI_API_KEY`, `OPENAI_BASE_URL`, and `OPENAI_MODEL`. A bare `OPENAI_API_KEY` falls back to OpenAI defaults, which can produce unauthorized/provider-mismatch errors when the key actually belongs to OpenRouter, a local proxy, or another compatible provider. If only model/base-url vars are present without a matching key, treat it as partial env config and do not use it.

Default setup should omit `--use-env-llm` and let Enzyme use hosted bootstrap/auth. If the user intentionally wants to use their own OpenAI/OpenRouter/OpenAI-compatible provider, verify only the presence of the needed env var names without printing values and pass `--use-env-llm`:

```bash
enzyme init --quiet --use-env-llm
enzyme refresh --quiet --use-env-llm
```

Treat localhost/`ENZYME_LOCAL_MODEL=1` as the local-model path. If provider intent is unclear, ask before running expensive catalyst generation.

Enzyme does not replace the user's memory system. It indexes the markdown structure the user already has: folders, tags, wikilinks, dates, inboxes, daily notes, people pages, and frontmatter. Preserve that structure and use it as retrieval signal.

User-facing mental model: Enzyme does the slow interpretive pass once, then leaves behind fast search handles. During init, it reads the shape of the vault and creates a small set of source-grounded questions for the ideas that keep showing up. Those questions are not summaries; they are questions the user's notes are good at answering. Later, when an agent needs context, it can use those precomputed questions to find relevant notes immediately instead of rereading the vault or guessing keywords. Refresh folds new markdown into that compiled map so future sessions can use it.

Do not expose embedding implementation details unless asked. Prefer simple language such as: "Enzyme turns your notes into questions an agent can search with," "the slow interpretive pass happens during init," and "runtime retrieval uses precomputed handles, so it is fast."

Do not build a separate context tree. Learn from the user's folders, but prefer lightweight markdown signals: tags for recurring ideas and wikilinks for people, projects, companies, decisions, and concepts. Create new folders or people pages only when the vault already uses that convention or the user asks for it.

Treat setup as an indexability assessment. A workspace may start anywhere on the spectrum from raw JSON exports to a highly structured agent-team markdown repo. Do not assume either is already Enzyme-ready. The final setup target is an Enzyme-indexable markdown workspace: meaningful folders, explicit dates when temporal retrieval matters, stable tags/wikilinks/frontmatter entity handles where the vault benefits from them, and enough source text for grounded retrieval. Use the audit to explain what is already indexable, what is missing, and what work would make Enzyme materially better before init. Ask for user feedback on that interpretation before writing config, materializing imports, or repairing structure.

## First-Time Setup

If `.enzyme/enzyme.db` is missing, do setup before normal retrieval. Setup should demonstrate value, not interrogate the user or impose a schema.

Import materialization and structural repair belong in setup/repair, not routine runtime note-writing. Use this phase flow:

1. **Assess indexability** — run `enzyme scan`; if raw exports or weakly structured markdown are present, use a scripted read-only audit to report what is already indexable, what is missing, and what would materially improve Enzyme.
2. **Preview the target shape** — explain the Enzyme-readable workspace you are aiming for: meaningful folders, dates when temporal retrieval matters, stable entity handles, scope boundaries, and source text.
3. **Materialize or repair only with approval** — required outputs are an audit summary, a dry-run plan or sample diff, and a backup plan. Preserve raw artifacts. Apply only deterministic high-confidence changes the user approves.
4. **Configure, initialize, validate** — tune TOML, run init/refresh, then test with petri/catalyze prompts that should cite the newly indexable captures.

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
- what is not yet Enzyme-indexable or would be weakly indexable, such as raw dumps, missing dates, missing entity handles, or scope boundaries that are only implicit;
- why the user does not need a new memory architecture;
- small habit upgrades, such as stable wikilinks for central people/projects/concepts and durable existing tags;
- the proposed stance for ongoing capture, durable work context, relationship/entity context, reference material, temporal context, and noise;
- 3-5 vault-specific prompts an Enzyme-aware agent should answer with grounded source notes;
- how the demo should show the map-to-connection loop: Enzyme's precomputed questions help the agent recognize active ideas, then source-grounded retrieval places notes beside each other so a useful question appears;
- any external corpora that could be searched with `enzyme catalyze --target`;
- if the vault would materially benefit, a minimal high-confidence retrieval repair offer before init, with exact scope and user confirmation.

Ask for corrections to that stance before writing config. Do not ask the user to classify the vault up front.

If offering a minimal repair, keep it small, reversible, and based on existing conventions: adding missing date frontmatter to date-named notes, adding `people:`/`projects:` fields only where those fields already exist and exact wikilinks are present, adding a few central wikilinks to obvious notes, or excluding runtime/generated folders from config. Do not move files, create a new taxonomy, generate summaries, or bulk-normalize the vault during first setup. If the user declines, initialize as-is and still demonstrate value; after the demo, offer the remaining refinements as optional next steps.

If the vault uses Obsidian, optionally offer capture templates after the demo. Core Obsidian Templates are enough for simple date/time templates inserted into a note, and Daily notes can apply a daily template. Do not require new plugins during Enzyme setup. Frame templates as optional capture affordances for future new notes, not prerequisites: they help users create inbox/daily/meeting/project notes with existing date/tag/wikilink handles so Enzyme can build on them later.

After `enzyme scan --write-config`, read `~/.enzyme/config.toml` and compare it with the scan evidence before `enzyme init`. Add missing important markdown folders as `folder:<path>` entries when they are central and not covered by a parent. Common folders include `inbox`, `daily`, `journal`, `docs`, `notes`, `research`, `logs`, `decisions`, `meetings`, `transcripts`, `sessions`, `projects`, `areas`, `resources`, `people`, `contacts`, `clients`, and `companies`. Keep the entity list focused; prefer scan-backed top-level surfaces over every subfolder.

For expandable folders, look for scan/petri evidence that a selected folder contains page files that are also wikilinks elsewhere (`page_entities`, `page_entity_children`, `sampled_children`, `catalyzed_children`, or similar folder-page evidence). In Rust Enzyme, selecting the parent folder is usually enough: the selection pipeline auto-detects expandable folders, caps/ranks children, and materializes child page links as their own catalyst entities. Do not manually add every child wikilink to config just because it lives in a selected folder; add a child link separately only when it has a distinct role independent of the parent folder.

Use profile overrides only when the posture is clear. Loose examples: people/relationships → `relational`; projects/work/inbox → `operational`; product/strategy/decisions → `decision_trace`; Readwise/research/PKM → `resonance_trace`; journal/daily/writing → `reflective`; faith/philosophy/tradeoffs → `tension_trace`; taste/feedback/activity logs → `preference_evidence`. Example TOML entity: `{ "folder:people" = { profile = "relational" } }`. Leave ambiguous entities un-overridden rather than guessing.

Keep runtime/build folders excluded: `.hermes`, `.enzyme`, `.git`, `.claude`, `.agents`, `.codex`, `.codex-work`, `.pi`, `.local`, `.obsidian`, `node_modules`, `target`, `dist`, `build`, and templates.

For persistent `targets = [...]` entries, use raw filesystem paths or vault-relative directory paths such as `Readwise/Books` or `../research`; do not prefix them with `folder:` and do not use tag/wikilink syntax.

For voice agents that need an immediate first turn, use:

```bash
enzyme init --voice-ready --voice-entities 3 --voice-min-catalysts 1
```

It returns once seed petri context exists; semantic search becomes available after the detached init worker finishes.

## Session Lifecycle

Explain setup/refresh simply when useful: init is the slow compile step that turns the vault into source-grounded questions; refresh is how new notes join that compiled map; normal retrieval is fast because the agent uses those precomputed handles instead of starting from scratch.

`enzyme refresh --quiet` JSON includes an `update` object with a two-state contract. Check `update.status`: `ok` means nothing to do (a `note` may explain a slower call such as vault regeneration); `action_required` means perform or relay the `action` string to the user — never interpret any other keys. Use `enzyme status` when you need full binary-update diagnostics.

Claude/Codex plugin installation does not install Petri or refresh hooks by default. Do not create `.claude/hooks/enzyme-petri.sh`, mutate `.claude/settings.json`, or rely on automatic prompt injection during normal setup. Use `enzyme petri` and `enzyme catalyze` explicitly when context is needed.

What's automatic depends on your runtime:

**Hermes** (hooks handle it):

- **First setup** — the plugin can bootstrap the binary; the workspace still needs `enzyme scan`, TOML validation, and `enzyme init` once
- **Session start** — binary bootstrap + `enzyme refresh` run automatically
- **Each turn** — `enzyme petri --query` injects vault context before the model sees your message
- **Session end** — after any useful markdown notes are written, `enzyme refresh` indexes them

**OpenClaw** (skill instructions + config):

- **Session start** — run `enzyme refresh --quiet` (add to AGENTS.md or heartbeat)
- **First turn / context-dependent turns** — run `enzyme petri --query "user's message"` before responding
- **Session end** — write useful markdown notes if the session produced durable memory, then run `enzyme refresh --quiet`
- **Between sessions** — heartbeat or cron can keep the index fresh for external syncs (see README for config)

In both cases: use petri and catalyze as tools. The difference is whether context injection is automatic in the host runtime (Hermes) or agent-driven from these instructions.

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

- Annotated reference/import vault (for example Readwise, web clips, papers, book notes, transcript highlights): start from one user annotation, marginal note, or explicit reaction when present, quote it as source text, then place it beside the saved passage and one adjacent source until a question appears. If the user names a title or distinctive phrase, use exact search to find that obvious note before using petri/catalyze for adjacent connections.
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
- Use exact search for names, source titles, distinctive phrases, `#tags`, `[[wikilinks]]`, and literal text.
- For annotated reference/import vaults, if the user names a title or phrase, find that obvious note first with exact search, then use petri/catalyze to connect it to adjacent material.
- `enzyme catalyze "query" --target ./target-dir` prepares external content using vault catalysts, then searches it.
- Tags can appear as `- tag` in frontmatter or `#tag` inline; search without `#` when you need both.

### External References

Use `enzyme catalyze "query" --target ./target-dir` when the user wants to draw from external material without merging it into the vault: Readwise exports, articles/books, transcripts, research dumps, client docs, code repos, converted PDFs, Discord/Slack exports, or downloaded archives. Enzyme prepares the target automatically on first use.

```bash
enzyme catalyze "query" --target ./target-dir
```

Target search projects the source vault's catalysts onto the target corpus:

```text
source vault catalysts → external target chunks
```

The vault is the lens. Search both sides when comparison matters:

```bash
enzyme catalyze "query"
enzyme catalyze "query" --target ./target-dir
```

Present it as: "I searched your own notes for the internal thread, then searched the external material through the same conceptual frame." Mention that target search may miss themes that exist only in the target and not in the user's vault.

## Writing Notes

Write runtime memory as ordinary markdown observations, not as a separate memory store. The point is to leave sparse, source-linked notes that Enzyme can refresh and retrieve through Petri and Catalyze when a durable conclusion, commitment, reframe, or open loop is worth carrying forward.

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
created: "[[YYYY-MM-DD]]"
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

For broad exploration, use petri plus 1-2 catalyze searches, then open one specific connection among the user's notes. Do not present a topic list. If exact search finds an obvious named note but catalyze misses it, say so plainly and use Enzyme for adjacent connections rather than pretending semantic retrieval found the note unaided.

For search results, do not lead with metadata. Notice repeated words, time gaps, changed wording, adjacent ideas, practical consequences, or source disagreements across results. End with one concrete next direction, not a generic invitation.

Choose the presentation posture from the user's task, not from the `catalyze` response:

- Exploration: wonder, probe, notice patterns, and open one specific connection.
- Continuity: restore what the user knew, show trajectory and stopping points, and enable forward motion.
- Reference/imports: surface what drew attention and connect imports to the user's own thinking without treating them as authoritative.
