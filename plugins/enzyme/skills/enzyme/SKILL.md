---
name: enzyme
description: >
  Set up Enzyme for an uninitialized Obsidian, Markdown, or Hermes agent
  workspace. Use when the workspace needs compatibility assessment, first-time
  scan, user-confirmed entity selection, TOML config validation, persistent
  agent instructions, optional apply targets, or onboarding demo prompts. Do not
  use for routine retrieval in an already initialized vault.
allowed-tools: Bash, Read, Glob, Grep, Edit, Write
argument-hint: [vault-path]
---

# Enzyme Setup

Use this skill only for first-time workspace setup or setup repair. If the vault is already initialized and the user only wants exploration, use the regular Enzyme runtime workflow instead.

For Hermes, this is an operational workspace setup path, not Hermes development. The goal is to leave the agent with a local Enzyme index, durable workspace instructions, and a concrete demonstration of how Enzyme makes natural markdown capture useful.

Enzyme bootstraps on top of the user's existing markdown system. Do not impose PARA, LLM Wiki, GBrain, a context tree, or an agent-written memory schema. Treat existing folders, inboxes, daily notes, tags, wikilinks, people pages, frontmatter, timestamps, and ordinary prose as retrieval signal. Prefer tiny habits over reorganization: stable wikilinks for central people/projects/concepts, durable tags for recurring themes, and normal notes in the vault's existing locations.

User-facing mental model: Enzyme does the slow interpretive pass once, then leaves behind fast search handles. During init, it reads the shape of the vault and creates a small set of source-grounded questions for the ideas that keep showing up. Those questions are not summaries; they are questions the user's notes are good at answering. Later, when an agent needs context, it can use those precomputed questions to find relevant notes immediately instead of rereading the vault or guessing keywords. Refresh folds new markdown into that compiled map so future sessions can use it. Do not expose embedding implementation details unless asked.

Treat setup as an indexability assessment. A workspace may start anywhere on the spectrum from raw JSON exports to a highly structured agent-team markdown repo. Do not assume either is already Enzyme-ready. The final setup target is an Enzyme-indexable markdown workspace: meaningful folders, explicit dates when temporal retrieval matters, stable tags/wikilinks/frontmatter entity handles where the vault benefits from them, and enough source text for grounded retrieval. Use the audit to explain what is already indexable, what is missing, and what work would make Enzyme materially better before init. Ask for user feedback on that interpretation before writing config, materializing imports, or repairing structure.

## Prerequisite

The Enzyme CLI binary must already be installed before this setup flow starts. If `enzyme --version` fails, install the binary first via Homebrew, the curl installer, or the host plugin's bootstrap path.

This skill should not normally call `enzyme install <runtime>`. That command is how a user installs/updates Enzyme's runtime skill and workspace instructions; if this skill is already loaded, that has already happened or the host plugin provided it. Mention `enzyme install <runtime>` only when the user explicitly asks how to install/update runtime instructions outside the current setup flow.

## Vault Path

Enzyme resolves the vault path in this order: `-p` flag > `ENZYME_VAULT_ROOT` env var > current directory.

Resolve the vault root once and run the workflow from that directory. For Hermes, prefer the directory where the user launches `hermes`; Hermes loads `.hermes.md`/`HERMES.md` and `AGENTS.md` relative to startup paths, so the workspace root matters.

Check initialization with `enzyme status` or by looking for `${ENZYME_VAULT_ROOT:-.}/.enzyme/enzyme.db` from the resolved vault root. On FUSE/Cowork filesystems Enzyme may mirror the active SQLite database into an app cache path, so do not rely on a fixed `/tmp/enzyme/enzyme.db` location.

If `enzyme.db` exists or `enzyme status` reports an initialized vault, do not blindly re-init. Audit the current config and explain whether a repair would require editing `~/.enzyme/config.toml` plus `enzyme refresh` or a deliberate re-init.

## Commands

```bash
enzyme scan                 # Preview suggested entities and exclusions
enzyme scan --write-config  # Create/update ~/.enzyme/config.toml after confirmation
enzyme init --quiet         # Initialize using confirmed TOML entities/profiles
enzyme petri                # Show the live entity/catalyst map
enzyme petri --query "..."  # Simulate a user prompt
enzyme catalyze "..."       # Show grounded source-note retrieval
enzyme apply ./target-dir   # Project this vault's catalysts onto external refs
enzyme catalyze "..." --target ./target-dir
enzyme status
```

Read Enzyme JSON directly. Do not pipe through Python or jq unless the output is malformed or too large to inspect. If `enzyme scan` output is too large for the agent interface, redirect it to a temporary file and read focused sections from that file; do not rerun broad filesystem audits just to compensate for display limits.

## Setup Workflow

Run setup as a phase-based indexability flow. The detailed steps below map to these phases:

1. **Assess indexability** — combine `enzyme scan` with bounded inspection. If raw exports, dumps, or weakly structured markdown are present, propose a scripted read-only audit.
2. **Preview the target shape** — explain what is already Enzyme-indexable, what is missing, and what markdown handles would make retrieval materially better.
3. **Materialize or repair only with approval** — for scripted work, required outputs are an audit summary, a dry-run plan or sample diff, and a backup plan. Preserve raw artifacts; do not infer uncertain aliases, rewrite source bodies, or bulk-summarize captures without explicit user request.
4. **Configure and initialize** — tune TOML from the confirmed stance, then run init/refresh.
5. **Validate** — use petri/catalyze prompts that should cite the newly indexable notes or captures.

### 1. Run scan preview

Run `enzyme scan` before init. Treat it as the primary setup evidence and proposal, not truth. Do not run `enzyme scan --write-config` yet.

The scan is structured for setup agents. Read it as a compact map of the vault rather than as a final answer.

### 2. Auth and provider safety

Let Enzyme decide when auth is needed. Do not preflight `~/.enzyme/auth.json` or start login before a command asks for it. If an Enzyme command reports that login is required, start device login in the background:

```bash
nohup enzyme login --json --no-open > /tmp/enzyme-login.log 2>&1 &
echo "Started login PID $!"
sleep 3
cat /tmp/enzyme-login.log
```

Read JSONL events directly. Show the verification URI/code when present, wait for `success`, `expired`, or `error`, then retry the original command. Never ask the user to paste API keys or auth tokens.

Do not silently spend a user's personal `OPENAI_API_KEY`/`OPENROUTER_API_KEY` just because it exists in the shell; Enzyme ignores inherited LLM env keys by default. If the user intentionally wants their own OpenAI/OpenRouter/OpenAI-compatible provider, verify only the presence of env vars without printing values, then run init/refresh with `--use-env-llm`:

```bash
enzyme init --quiet --use-env-llm
enzyme refresh --quiet --use-env-llm
```

### 3. Interpret scan evidence with bounded follow-up

Use the scan to understand the vault shape, then add judgment. Favor recurring, recent, human-meaningful folders/tags/links over raw frequency alone. Use samples to check whether a candidate actually means what its name suggests. Use child maps to avoid selecting both a parent and many near-duplicate children unless the child has a distinct role. Use frontmatter samples to preserve conventions, not to invent a schema.

For expandable folders, look for scan/petri evidence that a selected folder contains page files that are also wikilinks elsewhere (`page_entities`, `page_entity_children`, `sampled_children`, `catalyzed_children`, or similar folder-page evidence). In Rust Enzyme, selecting the parent folder is usually enough: the selection pipeline auto-detects expandable folders, caps/ranks children, and materializes child page links as their own catalyst entities. Do not manually add every child wikilink to config just because it lives in a selected folder; add a child link separately only when it has a distinct role independent of the parent folder.

Profile overrides are optional. Add them when the surface has an obvious stance; otherwise leave the entity un-overridden. As loose heuristics: people/relationships tend relational; projects/work/inbox tend operational; product/strategy/decisions tend decision_trace; Readwise/research/PKM tend resonance_trace; journal/daily/personal writing tend reflective; faith/philosophy/tradeoffs tend tension_trace; taste/feedback/activity logs tend preference_evidence.

If the scan is ambiguous, read a bounded amount of extra context: structural files such as `CLAUDE.md`, `AGENTS.md`, `README.md`, `MOC.md`, `Index.md`, or `_index.md` for conventions, and `sample_files` or a tiny number of scan-surfaced notes for meaning. Avoid recursive vault inspection unless the user asks or setup would otherwise be underdetermined.

Also note possible `enzyme apply` targets from the scan and filesystem shape: Readwise exports, article/book folders, transcripts, research dumps, client docs, code repos, PDFs converted to markdown, Discord/Slack exports, or downloaded archives.

For annotated reference/import markdown such as Readwise exports, web clips, papers, book notes, or transcript highlights, distinguish first-class handles from readable source text. Tags, folders, and wikilinks are Enzyme handles. Source-specific fields such as `Author::`, `Title::`, `Last-Highlighted-Date::`, or `Source-URL::` are useful evidence the agent can read, but they are not separate Enzyme schema fields unless represented as tags, links, folders, or frontmatter. User annotations, marginalia, or explicit reactions such as `**Note**:` are often the most recognizable prose in the vault; treat them as high-value source text for demos, not as a special index primitive.

If the workspace contains raw exports or dumps such as Granola JSON, ChatGPT exports, email archives, Slack/Discord exports, call transcripts, Readwise exports, or other unstructured data, treat them as import materialization candidates. The agent should reason about what the audit script needs to discover, not hand-inspect or hand-rewrite files one by one.

Only offer retrieval repairs or materialization when there is a high-confidence gap that would materially improve indexing. Do not move files, create a new taxonomy, generate summaries, or bulk-normalize the vault during first setup. Good Enzyme fits have durable markdown traces: decisions, research, project journals, meeting notes, transcripts, reading notes, reflections, or agent-written summaries. Weak fits are mostly binary assets, generated outputs, dependencies, or code without markdown traces.

### 4. Produce a setup preview, not an interview

Before writing config, show a concrete preview. Ask for corrections, not a questionnaire.

The preview must include:

1. **What is already working as signal** — observed folders/tags/wikilinks/dates/frontmatter from the scan, with counts and 1-2 concrete examples from `entity_samples`, `frontmatter_samples`, or `sample_files`.
2. **What is not yet indexable or is weakly indexable** — raw dumps, missing dates, missing entity handles, implicit project/client/person scopes, or generated/noisy folders that would make Enzyme less useful unless materialized, repaired, or excluded.
3. **Why more organization is not required** — Enzyme is designed to work with partial, living markdown structure.
4. **Why init/refresh matter** — explain simply that init does the slow compile step once, turning recurring vault ideas into source-grounded questions the agent can search with quickly later; refresh folds new notes into that compiled map.
5. **Small habit upgrades** — link central people/projects/concepts, reuse durable tags, preserve date/frontmatter conventions, avoid hidden memory files.
6. **Proposed Enzyme stance** — which surfaces are ongoing capture, durable work context, relationship/entity context, reference material, temporal context, and noise.
7. **Profile posture** — translate catalyst profiles into human language:
   - `relational`: people, relationships, clients, community, hospitality.
   - `operational`: projects, work, meetings, tasks, inbox/current execution.
   - `decision_trace`: product, strategy, founding, architecture, decisions.
   - `resonance_trace`: Readwise, references, research, libraries, PKM.
   - `reflective`: journal, daily, travel, writing, personal reflection, capture.
   - `tension_trace`: faith, philosophy, concepts, unresolved tradeoffs.
   - `preference_evidence`: taste, preferences, feedback, recipes, activities.
8. **Falsifiable outcomes** — 3-5 vault-specific prompts an Enzyme-aware agent should answer with grounded source notes.
9. **Minimal repair offer** — if the vault could be in better shape, propose the smallest high-confidence edit set that would improve retrieval before init, with exact files/counts and why it matters. If the vault is already in good shape, explicitly skip this.
10. **Import materialization opportunities** — raw exports or dumps that could become markdown captures, the high-level audit/materialization plan, and what entity/date/scope handles would make them Enzyme-readable.
11. **Handle vs source-text explanation for imports/references** — for annotated imports, explicitly say which structures are Enzyme handles and which are readable source text. Example: “source fields are readable text; folders/tags/wikilinks/frontmatter are handles; user annotations are high-value prose, not special primitives.”
12. **Apply opportunities** — external targets that could be searched through the vault's catalysts and what that would prove.

Example framing:

```md
This vault already has enough signal for Enzyme. I would treat `inbox/` as ongoing capture, `people/` and repeated wikilinks as relationship context, `Readwise/` as reference material, and `#enzyme`/`#founding` as decision/product threads. You do not need a separate memory tree. Enzyme will do the slower compile step once: it turns recurring ideas in ordinary markdown into source-grounded questions an agent can search with quickly later. Refresh is how new notes join that compiled map.

If this setup works, prompts like these should return grounded notes:

- ...
- ...

Correct this stance before I write config.
```

User confirmation is required before config is written or edited.

### 5. Offer a minimal retrieval repair only if it would help

If the audit found a small, high-confidence set of vault edits that would materially improve Enzyme's first demo or future retrieval, offer it before init. The offer must be concrete and bounded:

```md
I can make one minimal retrieval repair before init:

- add `created:` to 12 date-named meeting notes using their filenames
- add `people:` links to 6 notes that already mention existing `people/*.md` pages
- add `#decision` to 4 notes that already use the vault's decision-note pattern

This does not move files or create a new schema. It only makes existing structure more legible to Enzyme. Should I do this small pass before initializing, or initialize as-is?
```

Rules:

- Only offer this when the vault would actually benefit. If existing folders/tags/links/dates are already strong enough, say so and skip.
- Keep the first repair small, reversible, and reviewable.
- For broad repairs, use scripted audits/batches and present required outputs: audit summary, backup plan, dry-run plan or sample diff.
- Use only existing conventions or explicitly approved new ones.
- Preserve raw artifacts. Do not infer uncertain aliases, rewrite source bodies, bulk-summarize captures, create new people/company pages, or create a parallel memory tree without explicit user approval.
- If the user declines, initialize as-is and still demonstrate value.

After the demo, offer the remaining lower-priority refinements as optional next steps.

### 6. Persist and tune TOML before init

`~/.enzyme/config.toml` may not exist before init. To create a safe starting section, run after confirmation:

```bash
enzyme scan --write-config
```

Then read `~/.enzyme/config.toml` directly and edit the `[vaults."/absolute/path"]` section before `enzyme init`.

Validate and tune:

- Start from scan `entities` and tune using `top_entities` plus evidence fields; do not add entities absent from the scan unless you have read a specific file and can justify why scan missed it.
- Add important content folders as `folder:<path>` unless intentionally covered by a parent or excluded.
- Keep the entity list focused, usually 12-30 entities. Prefer top-level/high-signal surfaces over every tiny subfolder, and use `folder_children`/`tag_children` to collapse near-duplicates.
- Add profile overrides for important entities when scan evidence shows a clear posture. Leave ambiguous entities un-overridden rather than guessing.
- Set `min_top_catalysts` when a large/live entity needs enough catalyst coverage for the onboarding demo.
- Use scan `excluded_folders` as the baseline, and keep runtime/build folders excluded: `.claude`, `.agents`, `.codex`, `.codex-work`, `.conversations`, `.enzyme`, `.git`, `.hermes`, `.obsidian`, `.pi`, `.trash`, `.local`, `node_modules`, `target`, `dist`, `build`, `__pycache__`, and templates.
- Treat recurring frontmatter entity fields as note-writing conventions, not config entities.
- Treat apply targets as `targets = [...]` only when the user confirms the external corpus should persist as an applied target; otherwise demo `enzyme apply` manually later. `targets` entries are raw filesystem paths or vault-relative directory paths such as `Readwise/Books` or `../research`; do not prefix them with `folder:` and do not use tag/wikilink syntax.

Expandable folders are runtime-derived from selected folder entities. If `folder:people` has page-entity children, Rust may expand those children into separate catalyst entities during selection; children are not normally persisted in config.

Profile-aware TOML shape:

```toml
[vaults."/path/to/vault"]
min_top_catalysts = 40
excluded_folders = [".enzyme", ".obsidian", ".git", ".agents", ".pi", ".codex-work", "node_modules", "templates"]
entities = [
  { "folder:inbox" = { profile = "operational" } },
  { "folder:people" = { profile = "relational" } },
  { "folder:readwise" = { profile = "resonance_trace" } },
  { "#product" = { profile = "decision_trace" } },
  { "#pkm" = { profile = "resonance_trace" } },
  { "#journal" = { profile = "reflective" } },
]
targets = []
```

If the vault is already initialized and profile/entity choices were wrong, explain that profile changes affect future catalyst generation. Run `enzyme refresh --quiet` for minor changes; use a deliberate re-init when the catalyst set itself needs to be regenerated from a substantially different entity/profile stance.

### 7. Initialize

Run:

```bash
enzyme init --quiet
```

The quiet output may include petri data under `petri`; use it if present.

### 8. Demonstrate the map-to-connection loop

Onboarding is not complete until the user sees how Enzyme helps an agent recognize active ideas and then activate them as connections to source notes.

1. Run `enzyme petri` and summarize the active map in user language: name 3-5 active entities/themes, why they look live, and the kind of catalyst language attached to them. Frame this as "what a future agent can notice before it answers," not as a topic list.
2. Choose one of the preview's falsifiable prompts. Prefer a prompt that combines two active surfaces from the map, such as a person plus a project, a product thread plus a reference corpus, or a recurring tag plus a recent folder.
3. Run query-aware petri to show recognition before search:

   ```bash
   enzyme petri --query "<simulated user prompt>"
   ```

   Explain which active ideas the query pulled forward and what search vocabulary the map suggests.

4. Compose a catalyze query from the prompt plus petri catalyst vocabulary, then run:

   ```bash
   enzyme catalyze "<query composed from the prompt plus catalyst vocabulary>"
   ```

5. Present one first-value connection, grounded in source-note excerpts. Do not end with setup status or a topic list.

   Use this framing:
   - `A connection worth opening: <plain-language phrase>.`
   - Show 2-4 short excerpts or tight paraphrases from specific files, especially the user's own annotations or decision notes rather than only imported source text.
   - Put the excerpts beside each other with minimal interpretation:
     - `In <file>, you wrote...`
     - `Elsewhere, this shows up as...`
     - `Put together, the question becomes...`
   - Offer one concrete next move, such as following into a tag/file/source or comparing against a related note.

Prefer words from the user's own notes. Avoid performative meta-language such as "live thread," "you are circling," "tension," "resonance," or "emerging pattern" unless those are the user's words. Do not claim intimacy with the user; create recognition by staying close to the artifacts.

Choose the first demo connection by vault type:

- Annotated reference/import vault (for example Readwise, web clips, papers, book notes, transcript highlights): start from one user annotation, marginal note, or explicit reaction when present, quote it as source text, then place it beside the saved passage and one adjacent source until a question appears. If the user names a title or distinctive phrase, use exact search to find that obvious note before using petri/catalyze for adjacent connections.
- Project/work vault: place a decision, blocker, meeting note, or artifact beside a later note that changes its meaning or next step.
- Journal/daily vault: place two entries from different dates beside each other to show how the wording, stakes, or desired action changed.
- People/CRM vault: place context notes beside a recent interaction or commitment to reveal one concrete next step.
- Research vault: place sources that sharpen an assumption, disagreement, missing evidence, or possible synthesis.

Do not expose catalyst IDs, raw scores, or tool mechanics unless asked. The point is to show that Enzyme turns natural notes, wikilinks, tags, dates, and prose into an active map, then places source notes beside each other so a useful question appears. The demo succeeds only if it gives the user one specific, sourced connection they can recognize as theirs and one obvious next question to pursue. If the result feels generic, run another retrieval with sharper catalyst vocabulary and do not call setup complete yet. If exact search finds an obvious named note but catalyze misses it, say so plainly and use that as a retrieval limitation; do not pretend semantic retrieval found the connection unaided.

### 9. Demonstrate or propose `enzyme apply`

If the audit found a strong external reference candidate and the user approves, run:

```bash
enzyme apply ./target-dir
enzyme catalyze "<same or related query>" --target ./target-dir
```

Then compare the internal and external searches:

```md
I searched your own notes for the internal thread, then searched the external corpus through the same conceptual lens. Your notes say X; the outside material adds/challenges Y.
```

Use `apply` when the user wants to draw from external refs without merging them into memory. Explain the mechanism accurately:

```text
source vault catalysts → external target chunks
```

The vault is the lens. `apply` may miss themes that exist only in the target and not in the user's vault.

### 10. Offer remaining retrieval refinements

After the user has seen the map-to-connection demo, offer the rest of the optional improvements as next steps. Keep them ranked by impact and separate them from the successful initial setup.

Good candidates:

- broader date frontmatter backfill inferred from filename/path dates;
- note-level entity fields matched to existing wikilinks and existing vault conventions;
- `people:`/company/project fields for synced emails, transcripts, meeting notes, or CRM notes when central;
- scripted materialization of raw Granola, ChatGPT, email, Slack/Discord, meeting, or other JSON/export dumps into markdown captures with `created:`, `source:`, `raw_path:`, exact existing entity handles, and approved project/client/person scope boundaries;
- creating missing people/contact/company stubs only when repeatedly referenced and user-confirmed;
- deciding whether active inbox/transcript/import folders should remain first-class retrieval entities or become secondary/reference surfaces.

Present these as optional refinements, not prerequisites. Enzyme should have already demonstrated value on the existing knowledge base or the minimal confirmed repair set.

### 11. Offer Obsidian capture templates when relevant

If the vault uses Obsidian (`.obsidian/` exists), optionally offer to create or modify a small capture template after Enzyme has demonstrated value. This is not required for Enzyme and should not be framed as making the vault "correct." It is a capture affordance: it helps future new notes include the same lightweight handles Enzyme just used.

Research-backed constraints:

- The core Obsidian **Templates** plugin is enough for simple templates inserted into the active note. It supports `{{date}}`, `{{time}}`, and format strings like `{{date:YYYY-MM-DD}}`.
- The core **Daily notes** plugin can create/open today's note and apply a configured daily-note template.
- Preserve the user's existing capture workflow; do not require new plugins during Enzyme setup.

Be precise about what templates do. A template does not magically make existing notes better, and core Templates does not by itself decide where a new note goes. It helps when the user creates a new note through an Obsidian command, daily note, or their existing capture flow.

Offer examples based on the audit:

```md
Enzyme is already working. If you want, I can also add one lightweight Obsidian capture template so future notes naturally include the handles Enzyme benefits from.

This would let you create new inbox/daily/meeting/project notes with less structure-thinking at capture time. The template can preserve your existing date style and leave obvious slots for central wikilinks/tags, so Enzyme can later recognize active people, projects, decisions, and themes.

No new plugin is required for a simple template if Obsidian's core Templates plugin is enabled.
```

Possible templates:

```md
# Quick capture

---

created: {{date:YYYY-MM-DD}}
tags:

- capture

---

## {{title}}

What happened / what I noticed:

Why it might matter later:

Related:
```

```md
# Meeting / conversation

---

created: {{date:YYYY-MM-DD}}
people:

- projects:
- tags:
- meeting

---

## {{title}}

Context:

What changed:

Open loops:

Related:
```

```md
# Decision

---

created: {{date:YYYY-MM-DD}}
tags:

- decision
  projects:
- ***

## {{title}}

Decision:

Why:

Tradeoff / rejected alternative:

Related:
```

Only create or edit templates after user confirmation. Use the vault's existing template folder from `.obsidian/templates.json` when present; otherwise ask before creating `templates/`. Preserve existing template syntax; core Templates uses `{{date}}` and `{{time}}`.

### 12. Report setup result

Tell the user:

- initialized note/entity/catalyst counts;
- persisted entity list, profile stance, and exclusions;
- demo prompt used and what it proved;
- apply targets added or recommended;
- minimal repair performed or skipped, and remaining refinements offered for later;
- capture template created/updated or intentionally skipped.

End with the capture freedom message:

```md
You can keep capturing naturally. Enzyme gets better when durable notes use the vault's existing handles — stable wikilinks, durable tags, dates/frontmatter, and clear prose — not when you maintain a separate agent memory architecture. Optional templates can make those handles easier to include in new notes, but Enzyme works without them.
```
