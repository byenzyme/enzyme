---
name: enzyme-workspace-setup
description: Enzyme workspace setup, re-setup, and repair. Use when the user asks to set up Enzyme, diagnose an existing setup, repair retrieval, or clean up an old setup before reinitializing. Existing `.enzyme` files or installed runtime instructions are not proof that setup is complete. Diagnoses read-only against what Enzyme actually indexed, proves value on one real note, then proposes only minimal, consented repairs. Never edits note bodies. Do not use for routine search or retrieval when the user is only asking a question of an already healthy vault.
allowed-tools: Read, Glob, Grep, Bash, Write, Edit
---

# Enzyme Workspace Setup

The durable promise:

> You can start now. Existing files will not be changed until you say yes. If a repair is worth doing, it is small, reversible, and proved against the user's real notes.

Nothing but additive artifacts is written before the single consent gate: at most one proof note and temporary diagnostic/backup artifacts. A "no" ends cleanly, vault unchanged except for any proof note the user has already seen and accepted.

The first job of everything you say is revelation, not reassurance: show the user **what Enzyme anchored on in their vault** — the folders, tags, links, and running logs where their thinking accumulates — and **what each anchor makes possible**. A little mechanism is welcome when it makes the value believable ("for each anchor it keeps standing questions drawn from your own words; those are what answer you later"). Name every anchor in the user's vocabulary with their real folder names, counts, and dates, and pair it with a question it newly makes answerable: "it reads `content-drafts` as a timeline now, so your June draft and this morning's entry are different moments — ask what changed between them." Verdicts like "healthy" or "needs repair" are the footnote to this reveal, never the headline.

Speak in terms of what the user sees in their own files, not engine internals. Engine evidence guides you; the user hears it translated. Numbers are fine, internals are not: avoid DB column names, doctor cause tokens, "dated fraction," "embedding budget," "catalyst," "indexed," or "checksum" in user-facing copy. Say what it means: "this note's date is not being read," "the engine stores this twice," "I checked every file came back exactly as it was." Before sending any user-facing message, reread it as the user: rewrite every phrase they could not say back to you in their own words.

## Inputs

The user prompt should provide:

- Vault or workspace path.
- Optional preferred destination for new notes.
- Optional `ENZYME_BIN`; otherwise use `enzyme` from PATH.
- Optional existing setup constraints, such as "do not edit before init" or "use my provider key."

Treat the provided path as the workspace root unless the evidence or user says the notes live elsewhere.

If the source is a SQLite database rather than a Markdown folder, use the `enzyme-sqlite-source` skill. It explores the schema read-only, obtains confirmation on a universal role binding, and preserves the resulting contract and workspace state.

Existing setup is not a skip condition. If `.enzyme/`, `AGENTS.md`, `CLAUDE.md`, `.agents/skills/`, or `.claude/skills/` already exist and the user asked for setup, re-setup, diagnosis, or repair, inspect them as evidence of the current state, then continue through the diagnosis flow. Overwrite stale Enzyme runtime instruction sections only by rerunning `enzyme install <runtime>` when the user is trying to update installed instructions; otherwise repair setup state through the diagnosis, config, init, refresh, and proof workflow below.

## Two Questions Carry The Skill

Everything below is derived from asking these two questions honestly. Prescribe yourself no procedure you cannot justify from them.

### 1. Diagnosis: What Does Enzyme Actually See?

Ground truth is Enzyme's view, not the folder's appearance. Do not tier a cluster from a file count or a folder name. A `meetings/` folder can be stale exports; a `notes/` folder can be source code.

Run `"$ENZYME_BIN" doctor` first when available and account for every fact it reports before tiering anything. For every claim about what Enzyme can leverage, back it with what Enzyme actually extracted. When needed, inspect `.enzyme/enzyme.db` and compare representative files in each cluster against the indexed view, especially source path, date source, and parsed date.

A claim you can show against the doctor report or DB is a finding. A claim you can only reason toward is a hypothesis; verify it before stating it, or label it plainly as unverified.

Every gap between how the vault looks and what Enzyme sees is a finding. The mechanism facts below explain why gaps appear. They are a lens, not a checklist. If nothing rises above "leave it alone," say **"No restructuring needed; your vault already compounds well"** and stop. Never manufacture a finding to fill a template or justify a restructure.

**The normative model:** Enzyme compounds on a dated log. Notes with real dates, shared homes, and consistent entity handles stack on each other over time. A large folder of continuously dated notes is healthy infrastructure, not a mess to clean up. Content that is undated or carries metadata the parser cannot read is where compounding stalls. Duplicates waste the budget of what gets embedded. Spread across folders matters only when it splits a dated series or scatters an entity's handles apart, never as tidiness for its own sake.

Mechanism facts:

- Discovery reads Markdown. Anything saved only as PDF, transcript, export, image, JSON, or log is invisible to indexing unless there is meaningful Markdown wrapping it.
- Frontmatter is a free entity handle: title, dates, tags, and wikilinks can become signal, but only if the metadata parses. A leading `---` proves nothing.
- Retrieval compounds when related notes share one home and dedupes by file; scattered notes cannot stack as strongly.
- Undated notes fall out of time-aware ranking, and only the most-recent slice is embedded; old or undated content can go dark.
- Frequency and recency rank entities, so a large recent generated/reference/template folder can outrank real notes. Treat authored reference material as real content unless evidence says otherwise.
- Two shapes both look like "a log" but need opposite handling. **Many dated files** — one note per day in a folder — are the healthy shape: each carries its own date and stacks over time; leave them alone. **A single file of dated entries** — a `journal.md` that grows by appending, or a generated transcript with day-by-day sections — is different: without recognition Enzyme would read it as one note on one date, collapsing its internal timeline. Enzyme auto-recognizes this shape structurally and broadly: date headers may be ISO (`2026-07-28`), numeric (`07/28/2026`), prose (`July 28, 2026`), or weekday-prefixed (`Monday, July 28, 2026`), in mostly one chronological direction, with at least three entries that have real content beneath their headers (a page that merely *lists* dates, a metrics table, or an essay citing dates does not qualify). Recognized files are read as internal timelines automatically. What recognition looks like to the user: `scan --write-config` records each recognized file as a `log:` line in their settings, next to the folders and tags — a legible ledger of what Enzyme noticed. Removing a line withdraws the file from anchor treatment; adding one promotes a log the detector missed or judged too small. Enzyme never splits or edits the file itself, and never writes recognition into the note's own metadata.

Before you read anything, snapshot the workspace to a unique temporary path with `mktemp`; after diagnosis, re-snapshot and prove byte identity, then clean up both snapshots. Diagnosis that leaves residue was not read-only.

Place every observed cluster in exactly one tier:

1. **Already usable**: structured Markdown, dated notes, existing wikilinks or frontmatter that Enzyme can leverage now.
2. **Weakly indexable**: real content Enzyme can only partly see, such as missing dates, malformed metadata, thin bodies, or non-Markdown source.
3. **Would improve retrieval**: content that would compound much better after a small, deterministic move, exclusion, date stamp, or settings recording (a `log:` line).
4. **Can wait**: non-note structure that should not be reorganized for Enzyme, such as source code, raw exports, build outputs, and foreign-domain material.

Raw transcripts and evidence bundles stay put. Never move, edit, or summarize raw JSON, logs, transcripts, or exports during setup. Never bulk-invent `people:` or `tags:` frontmatter from body inference.

### 2. Mutation: Can Every Promise Be Proven Mechanically?

State each invariant, then prove it:

- **Read-only diagnosis**: prove zero filesystem residue with the before/after snapshot.
- **No body edits**: capture each note body's hash before and after any frontmatter stamp and abort if it would change.
- **Full backup before first mutation**: create a self-sufficient backup of every affected file before moving or stamping it. A temporary backup workspace is acceptable if it is retained through verification and the user is told where it lives; a vault-local backup is acceptable when the product surface needs a visible revert package. Do not rely solely on git.
- **Revertible**: run the revert end to end yourself and prove the notes come back byte-identical before claiming success.

Only moves/renames, date-repair frontmatter stamps, and settings edits (`log:` lines, exclusions, targets in Enzyme's config) are allowed, and only from deterministic evidence such as existing metadata, filenames, or exact entity matches. No body rewrites, invented aliases, bulk summaries, or inferred tags. Never write or print a secret.

Log recognition is a **settings edit, never a note edit**. When a single file of dated entries is confirmed (see the two-shapes mechanism fact), the remedy is its `log:` line in Enzyme's config: keep the line `scan --write-config` already recorded, or add one for a real recurring log the detector missed (too few entries, say). Narrate the lines at the gate in the user's terms — "Enzyme recognized these files as running logs and reads each as a timeline; here are their lines, remove any I misread" — and treat removal as a first-class correction, not a failure. Because a false recognition is one deletable line in one settings file, this is the lightest mutation in the skill; it still passes through consent because it changes what Enzyme anchors on. Do not stamp `type:` metadata into the user's files to mark logs — the engine honors such stamps as a legacy escape hatch, but recognition the user can see and curate belongs in their settings, not buried in per-file metadata. Never split or rewrite the file to normalize it — instead, if the user wants the healthier shape over time, offer "one file per day" as a future-capture habit at the close, not as a mutation now.

Never delete a user's file, not even an exact duplicate. If a duplicate must be retired, move it to a backup/retired location with a non-Markdown suffix and record the move so revert restores it.

## Connect

Resolve the Enzyme binary:

```bash
ENZYME_BIN="${ENZYME_BIN:-enzyme}"
"$ENZYME_BIN" --version
```

If the binary is missing, stop and explain that Enzyme must be installed or `ENZYME_BIN` must point to a working binary. Do not hunt through the filesystem for unrelated binaries unless the user asks.

Setup/re-setup command path, after the vault path is confirmed:

```bash
"$ENZYME_BIN" -p "<vault path>" scan
# Use scan output plus doctor/DB evidence to diagnose what Enzyme can see.

"$ENZYME_BIN" -p "<vault path>" scan --write-config
# Read ~/.enzyme/config.toml. This is the vault's anchor ledger: the entities
# (folders, tags, links) and any recognized running logs (`log:` lines) that
# Enzyme will anchor on. Tune only the minimum entity/log/exclusion/target
# choices supported by scan/doctor evidence before init, and carry the
# resulting anchors into what you narrate at the gate.

"$ENZYME_BIN" -p "<vault path>" init --quiet
"$ENZYME_BIN" -p "<vault path>" petri
"$ENZYME_BIN" -p "<vault path>" petri --query "<one real setup proof prompt>"
"$ENZYME_BIN" -p "<vault path>" catalyze "<query composed from the prompt and petri vocabulary>"
```

Do not skip `scan --write-config` and config review just because `.enzyme/` already exists. Existing config and index state are evidence to inspect; they are not proof that the setup is healthy or current.

Use `--use-env-llm` only when the user explicitly asks to use their own OpenAI, OpenRouter, or OpenAI-compatible provider and has set the complete provider environment. Do not inspect, print, unset, or rely on API-key environment variables during normal hosted setup.

Never delete `~/.enzyme/auth.json` or run `"$ENZYME_BIN" logout` without explicit confirmation.

If the configured path is empty, scratch, or not where the notes live, stop and ask for the correct folder before doing anything else.

## Prove It On One Real Note

Between diagnosis and the gate, you may additively create exactly one new note distilled from the user's own best existing content, if doing so would prove Enzyme's value. Lead with the user's own words and file attribution. Place two to four short real excerpts beside each other. End with one concrete next question.

This proof note edits and moves nothing. If there is not enough content for a rich proof note, produce the smallest honest starting point or skip it; never fabricate to fill it.

## Coach The Missing Handles

Some gaps no deterministic mutation can close: the handle Enzyme needs does not exist yet. Nothing recurs by name, dates live nowhere the parser reads, one thread is split across three spellings of a person. The rules above rightly forbid inventing that structure — so for these gaps the honest lever is coaching: change how tomorrow's notes are captured, never what yesterday's say.

Coaching lines use the anchors-reveal grammar in future tense: name the thread in the user's own content, one sentence of why Enzyme cannot grab it, the smallest capture habit that would create the handle, and the question that becomes answerable once it exists. The feel:

- Dates: "Your product thinking is all here, but nothing says *when* — every file claims the day of your laptop migration. Put the date in new filenames and each note becomes a moment that stacks; 'how did my thinking change after the pilot' starts being answerable."
- Homes: "'Pricing' lives in a dozen notes scattered at the root. I find each mention, but they never accumulate into a place that can be watched. Give the thread one home — a folder, or one running file of dated entries — and it becomes an anchor with its own standing questions."
- Names: "You write this person three different ways — three thin threads instead of one thick one. I won't rewrite the past, but pick one handle from today and the thread compounds from here."
- Trapped content: "The decisions from these meetings live in PDFs that can't be read as notes. Three Markdown lines per meeting — date in the title, what was decided — give more than the whole export."

Bounds: coach only where a tier-2 or tier-3 finding exists, offer at most two habits (the two that unlock the most), keep each to two sentences, and never present a habit as a condition of proceeding. Coaching is advice at the close, not a mutation and not a gate item.

## One Decision

Everything before this changed nothing except any additive proof note. A no ends cleanly with the vault unchanged. Present the gate as one narrated moment the user could read aloud in under a minute, opening with the anchors reveal:

1. **What Enzyme anchored on and what that buys you**: the folders, tags, and running logs it grabbed, each named in the user's vocabulary and paired with a question it newly makes answerable.
2. **Where Enzyme cannot leverage your vault yet, and why**, in plain user-visible terms.
3. **Exactly what I would move, exclude, or record in your settings**, with concrete paths, lines, and fields.
4. **The backup promise**, including where the backup/revert package will live for this run.
5. **The revert story**: "I will check that every affected file comes back exactly as it was."

When the plan mixes work of clearly different weight, offer at most two separately acceptable pieces, such as "just the duplicate cleanup" and "the broader re-file." A partial yes runs only the accepted piece under the same backup contract.

For the healthy-vault branch, the anchors reveal **is** the message: show what Enzyme anchored on and the value that flows through each anchor, close with **"No restructuring needed; your vault already compounds well,"** keep any proof note, and skip the gate. A healthy vault deserves the richest reveal, not the shortest one — the user should still leave knowing exactly which of their places Enzyme is watching and what to ask it.

## Restructure After Consent

Runs only after the single yes. The hard bound is moves/renames within reason, batch date-repair frontmatter edits, and Enzyme settings edits (`log:` lines, exclusions) only. Bodies are never touched, and log recognition never writes to note files at all.

Before the first mutation, write a backup package either to a unique temporary directory or to a visible vault-local directory selected by the product surface. It must contain:

- `sidecar/`: byte-for-byte copies of every file the repair will touch, preserving relative paths.
- `manifest.json`: every move, every frontmatter change, every sidecar path, and before/after body hashes.
- `pre.shasum`: snapshot of the notes before mutation.
- `revert.sh`: a script that restores from sidecar files and reverses moves.

The revert script must:

- Print what it will do before doing it.
- Replay moves in reverse.
- Restore changed files by copying original bytes from `sidecar`, not by reconstructing diffs.
- Keep the optional proof note unless the user explicitly asked to remove it.
- Compare only the user's notes that the repair owns; exclude app-owned state such as `.enzyme/`, `.git/`, product-specific state directories, caches, and the backup package itself.
- Exit non-zero if any owned note failed to restore.

After a successful repair, refresh Enzyme and report back using the same frame you opened with: what already worked, what changed, and up to two capture habits from the coaching section that would help the handles you could not create. If `enzyme refresh --quiet` returns `update.status == "action_required"`, perform or relay the `action` string before continuing.
