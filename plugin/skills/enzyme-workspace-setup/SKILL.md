---
name: enzyme-workspace-setup
description: Diagnose a Margins project folder read-only against what Enzyme's engine actually indexed, prove its value on one real note, then — with one consent — restructure moves + frontmatter under a byte-for-byte revertible backup. Never edits note bodies.
allowed-tools: Read, Glob, Grep, Bash, Write, Edit
---

# Enzyme Workspace Setup

The durable promise you are keeping to the user:

> You can start now. Here is the least surprising place for new meeting notes. Existing files will not be changed until you say yes — and if you do, one command puts everything back.

Nothing but additive artifacts is written before the single consent gate: exactly one proof note, and the pre-mutation backup. A "no" ends cleanly, vault unchanged, proof note kept. Speak throughout in terms of what the *user* sees in their own files — never engine internals. Engine evidence guides *you*; the user hears it translated. Numbers are fine, internals are not: no DB column names (`created_source=frontmatter`), no "dated fraction," "embedding budget," "indexed," or "checksum" in user-facing copy — say what it means instead ("this note's date isn't being read," "the engine stores this twice," "I verified every file came back exactly as it was").

## Two questions carry the whole skill

Everything below is derived from asking these two questions honestly. Prescribe yourself no procedure you can't justify from them.

### 1. Diagnosis — "What does the engine actually see here, and can you show the evidence?"

Ground truth is the engine's view, never the folder's appearance. Don't tier a cluster from a file count or a folder name — those lie (a `meetings/` folder can be stale exports; a `notes/` folder can be source code).

**Interrogate the engine directly — do not eyeball, do not assert.** Run `"$ENZYME_BIN" doctor` first and account for every fact it reports — dated fraction, undated content, duplicates, spread — before you tier anything. For every claim you make about what the engine can leverage, back it with what the engine actually extracted: query the indexed DB and compare its view to the human view. Concretely, after `init`, open `.enzyme/enzyme.db` and inspect the `docs` table — `source_ref`, `created_source`, `created_at` — for representative files in each cluster. This is the single highest-value diagnostic move in this skill; the gaps this fixture is built around only appear when you look at the engine's extracted view, not the file's surface. A claim you can *show* against the DB is a finding; a claim you can only *reason* toward (e.g. "the engine probably can't use this date format") is a hypothesis you MUST verify against the DB before stating it, or label plainly as unverified.

Every gap between how the vault looks and what the engine sees is a finding — you did not invent it, you surfaced it. The mechanism facts below explain *why* gaps appear. They are a lens, not a checklist to match. If nothing rises above "leave it alone," say **"No restructuring needed — your vault already compounds well"** and stop. Never manufacture a finding to fill a template or justify a restructure — no invented noise folder, no fabricated destination ambiguity, no reorg on a healthy vault. When two destinations are genuinely both plausible, present the ambiguity as ambiguity; when only one destination is configured or evidenced, do NOT conjure a choice. Under pushback, hold an honest line — do not conjure a finding to satisfy the user.

**The normative model — the lens that turns doctor's facts into judgment.** Enzyme compounds on a dated log: notes with real dates, shared homes, and consistent entity handles stack on each other over time. Read every fact doctor reports against that model. A large folder of continuously dated notes is healthy infrastructure, not a mess to clean up — an inbox holding a third of the vault at a high dated fraction is the ideal shape, not a problem. Content that is undated or carries metadata the parser can't read is where compounding stalls, so that is where attention belongs. Duplicates waste the budget of what gets embedded. Spread across folders matters only when it splits a dated series or scatters an entity's handles apart — never as tidiness for its own sake. Judge by whether the vault compounds, not by whether it looks neat.

Engine-mechanism facts (the lens):
- Discovery reads `**/*.md` only. Anything saved as PDF, transcript, export, image, JSON, or log is **invisible** to indexing. Compare the markdown count to the total file count; a wrapper `.md` that only links to non-md content is still zero signal.
- Frontmatter is a free entity handle (title, dates, `people`/`author`/`related` wikilinks) — **but only if it parses.** A leading `---` proves nothing: malformed metadata (a tab in a list, an unescaped colon, an unclosed quote) makes the parser return nothing and silently discards *every* field. **Don't trust a first-line check, and don't trust a `yaml.safe_load` you aimed at the wrong files.** Confirm against the engine's extracted view: a file whose `created_source` is `filename`/`filesystem` while it visibly carries a `created:` header is a parse failure the engine has already told you about — that mismatch IS the finding. Conversely, before calling a date format "opaque" (e.g. a `created: "[[2026-06-02]]"` wikilink), CHECK the DB — the engine may parse it fine (`created_source=frontmatter`), and asserting otherwise unverified is a false-confidence failure.
- Retrieval compounds when related notes share one home and dedupes by file; scattered notes can't stack. Undated notes fall out of time-aware ranking, and only the most-recent slice is embedded — old or undated content goes dark.
- Frequency-and-recency ranks entities, so a large recent noise/reference/template folder can outrank real notes. You *may* move or exclude such a folder — but this is a lens, not a mandate: real authored content (design notes, case studies, transcripts kept as reference) is NOT noise, and treating it as noise is over-diagnosis. Note the honest limit: search draws on an embedded chunk index the skill can't reach, so a generic skeleton note may still surface even after you exclude its folder. Say that plainly rather than overpromising.

Before you read anything, snapshot the workspace to a **unique-per-run** path (e.g. `mktemp` — never a fixed path a parallel run could clobber and fake a diff); after diagnosis, re-snapshot and prove byte-identity, then clean up both snapshots. Diagnosis that leaves residue was not read-only.

Place every observed cluster in exactly one tier and present the finding as: what the vault already does well (tier-1); real content the engine only partly sees and why, in plain language (tier-2/3); non-meeting structure to leave alone — source code, RFCs, raw exports, foreign-domain notes (tier-4); and — only if warranted — the one move or stamp that would unlock retrieval. That is the gate's diagnosis; there is no fixed template beyond these beats and the consent ask. Raw transcripts and evidence bundles stay put — never move, edit, or summarize a raw `json`/`err`/transcript file, and never bulk-invent `people:`/`tags:` frontmatter from body inference (entity inference is not safe).

### 2. Mutation — "Is every promise proven mechanically before I assert it?"

State each invariant, then prove it. Don't recite an incantation — the proof is the point.

- **Read-only** ⇒ prove zero filesystem residue (the unique-per-run before/after snapshot above).
- **No body edits** ⇒ prove hash equality: capture each note body's hash before and after any frontmatter stamp and abort if it would change. "Restructure changes where a file lives and its frontmatter header — never a word of the body."
- **Full backup before the first move — vault-local and self-sufficient without git.** Write everything to `.margins/restructure/<timestamp>/` inside the vault (NEVER to `/tmp` — off-vault backups are undiscoverable and leave residue): `sidecar/` (a byte-for-byte copy of every file the restructure will move or re-stamp, preserving relative paths — this is what revert restores from, not a reconstructed diff), `manifest.json` (every move, every frontmatter change, every sidecar path, before/after body hashes — and it MUST match the final live state; do not re-apply different values after a revert-test), `pre.shasum` (the pre-mutation snapshot), and `revert.sh`. Commit a git checkpoint too if it's a repo, but never depend on it.
- **Revertible** ⇒ *run the revert end-to-end yourself* and prove the notes come back checksum-identical **before** you tell the user it works. (To the user, this proof is worded as "I checked that every file comes back exactly as it was" — "checksum" is your word, not theirs.) Restore frontmatter by copying the whole file from its sidecar, not by reconstructing it. The revert keeps the proof note. **Scope the checksum comparison to the user's notes only** — exclude app-owned state (`.enzyme/`, `.margins/`) that a legitimate post-restructure refresh re-indexes and that the revert does not own, and exclude the additive proof note. A byte-correct revert that only "differs" in re-indexed app-state or a kept proof note must report success. Exit non-zero only when a real *note* failed to restore. `revert.sh` must derive the vault path from its own location (not a hard-coded absolute path), so running it from a copied or moved backup never writes into the wrong vault. If your first run emits any warning, disclose it, fix it, re-run — only then claim done.

Only moves/renames and frontmatter stamps, from deterministic evidence (dates from metadata or filenames, exact entity matches). No body rewrites, no invented aliases, no bulk summaries, no inferred `people:`/`tags:`. Never write or print a secret (including partial API-key handles). After a restructure, refresh the engine and report back in the same what-works / what-changed frame you opened with.

**Never delete a user's file — not even an exact duplicate.** Retire duplicates by moving them to `.margins/restructure/<timestamp>/duplicates/<original relative path>.bak` — the `.bak` suffix keeps them out of the engine's `*.md` discovery, the move is recorded in the manifest like any other, and the revert restores them. The promise to the user is literal: nothing you wrote ever disappears.

## Prove it on one real note (before the gate)

Between diagnosis and the gate, additively create exactly **one** new note distilled from the user's *own best existing content* — real excerpts with attribution, led by their own words, ended on their own open question. Weight the user's own thinking over raw source text. Place it in the destination the user actually configured (e.g. the `meetings/` folder they named), not a folder you improvised. If content is thin, produce the smallest honest starting-point note and say so; never fabricate to fill it. This edits and moves nothing. Show the user the product, not the plumbing.

## One decision — the single consent gate

Everything before this changed nothing except the additive proof note. A no ends cleanly, vault unchanged, proof note kept. Present it as one narrated moment the user could read aloud in under a minute: counts and one or two named examples, not exhaustive tables — offer the full file-by-file list only if they ask. **Before sending any user-facing message, reread it once as the user** — a writer who has never seen a database — and rewrite any sentence they couldn't say themselves: "indexed" → "the engine has read it", "embedding twice" → "storing it twice", "verify checksums" → "check every file came back exactly as it was", "sidecar"/"canonical" → name the actual file, "frontmatter" → "the note's header" (unless the user's own notes use the word). The reread is the rule; the examples are just the pattern.

1. **What your vault already does well** (tier-1 as affirmation).
2. **Where Margins can't leverage it yet, and why** — each finding transitions into its mechanism in plain user-visible terms ("these 40 notes live in six folders — retrieval stacks when related notes share a home"). Describe folders by what's in them, never by judging their names or tidiness.
3. **Exactly what I'd move and stamp** — concrete moves, concrete frontmatter fields. Bodies never touched. If no destination clearly wins, state your lean and let the user override; if only one destination fits, don't manufacture a choice.
4. **The backup promise** — a full copy of every affected file plus a change log written first, inside `.margins/restructure/<timestamp>/`.
5. **The revert story** — "one command puts every file back exactly as it was."

When the plan mixes work of clearly different weight — light cleanup (retiring duplicates, stamping a few dates) versus re-filing the user's folder layout — offer them as two separately acceptable pieces of the same moment: "both," "just the cleanup," or "no." Never more than two pieces, never a checklist of micro-consents. A partial yes runs only the accepted piece under the same backup contract.

For the healthy-vault branch, replace beats 3–5 with one line: *no restructuring needed — your vault already compounds well* — show the tier map as confirmation, keep the proof note, skip the gate. Never invent problems to fill the template.

## Margins integration realities

- The setup prompt provides `ENZYME_BIN` — an absolute path to an engine Margins already installed and verified. Use `"$ENZYME_BIN"` for every call; sanity-check it runs once. If it's unset or broken, do not hunt for or install a binary and do not delete auth or run `logout`. The single recovery path: tell the user to re-run **Setup with agent** in Margins.
- Initialize with `"$ENZYME_BIN" -p "<project folder>" init --quiet` (additive-only — writes into `.enzyme/` and possibly `~/.enzyme/config.toml`; a sandboxed global-config failure is non-fatal once `enzyme.db` exists). Init before diagnosis is fine. Only surface `--use-env-llm` if the user explicitly asks to use their own provider; do not inspect, print, or unset API-key variables.
- If a stage that needs an LLM is blocked (e.g. an OpenRouter 403 / quota / rate-limit), disclose it honestly as a timing/billing blocker scoped to that stage, tell the user to re-run `refresh` after the key resets, and do NOT invent structural work to compensate or attach a false causal story to the count.
- `.margins/` and `.enzyme/` are app-owned state; treat them as not-yours.
- If the configured folder is empty, scratch, or not where the notes live, **stop and ask** — confirm the real folder. Never run `projects add` through the engine; relocation happens by re-running Margins setup on the right project.
