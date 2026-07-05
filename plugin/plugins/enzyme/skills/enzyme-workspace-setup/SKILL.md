---
name: enzyme-workspace-setup
description: One-time Enzyme workspace setup. Use only for first-time setup of an uninitialized vault, or when retrieval seems broken and the user asks to re-diagnose. Diagnoses read-only against what Enzyme actually indexed, proves value on one real note, then proposes only minimal, consented repairs. Never edits note bodies. Do not use for routine search or retrieval in an already initialized vault.
allowed-tools: Read, Glob, Grep, Bash, Write, Edit
---

# Enzyme Workspace Setup

The durable promise:

> You can start now. Existing files will not be changed until you say yes. If a repair is worth doing, it is small, reversible, and proved against the user's real notes.

Nothing but additive artifacts is written before the single consent gate: at most one proof note and temporary diagnostic/backup artifacts. A "no" ends cleanly, vault unchanged except for any proof note the user has already seen and accepted.

Speak in terms of what the user sees in their own files, not engine internals. Engine evidence guides you; the user hears it translated. Numbers are fine, internals are not: avoid DB column names, doctor cause tokens, "dated fraction," "embedding budget," "catalyst," "indexed," or "checksum" in user-facing copy. Say what it means: "this note's date is not being read," "the engine stores this twice," "I checked every file came back exactly as it was." Before sending any user-facing message, reread it as the user: rewrite every phrase they could not say back to you in their own words.

## Inputs

The user prompt should provide:

- Vault or workspace path.
- Optional preferred destination for new notes.
- Optional `ENZYME_BIN`; otherwise use `enzyme` from PATH.
- Optional existing setup constraints, such as "do not edit before init" or "use my provider key."

Treat the provided path as the workspace root unless the evidence or user says the notes live elsewhere.

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

Before you read anything, snapshot the workspace to a unique temporary path with `mktemp`; after diagnosis, re-snapshot and prove byte identity, then clean up both snapshots. Diagnosis that leaves residue was not read-only.

Place every observed cluster in exactly one tier:

1. **Already usable**: structured Markdown, dated notes, existing wikilinks or frontmatter that Enzyme can leverage now.
2. **Weakly indexable**: real content Enzyme can only partly see, such as missing dates, malformed metadata, thin bodies, or non-Markdown source.
3. **Would improve retrieval**: content that would compound much better after a small, deterministic move, exclusion, or frontmatter stamp.
4. **Can wait**: non-note structure that should not be reorganized for Enzyme, such as source code, raw exports, build outputs, and foreign-domain material.

Raw transcripts and evidence bundles stay put. Never move, edit, or summarize raw JSON, logs, transcripts, or exports during setup. Never bulk-invent `people:` or `tags:` frontmatter from body inference.

### 2. Mutation: Can Every Promise Be Proven Mechanically?

State each invariant, then prove it:

- **Read-only diagnosis**: prove zero filesystem residue with the before/after snapshot.
- **No body edits**: capture each note body's hash before and after any frontmatter stamp and abort if it would change.
- **Full backup before first mutation**: create a self-sufficient backup of every affected file before moving or stamping it. A temporary backup workspace is acceptable if it is retained through verification and the user is told where it lives; a vault-local backup is acceptable when the product surface needs a visible revert package. Do not rely solely on git.
- **Revertible**: run the revert end to end yourself and prove the notes come back byte-identical before claiming success.

Only moves/renames and frontmatter stamps are allowed, and only from deterministic evidence such as existing metadata, filenames, or exact entity matches. No body rewrites, invented aliases, bulk summaries, or inferred tags. Never write or print a secret.

Never delete a user's file, not even an exact duplicate. If a duplicate must be retired, move it to a backup/retired location with a non-Markdown suffix and record the move so revert restores it.

## Connect

Resolve the Enzyme binary:

```bash
ENZYME_BIN="${ENZYME_BIN:-enzyme}"
"$ENZYME_BIN" --version
```

If the binary is missing, stop and explain that Enzyme must be installed or `ENZYME_BIN` must point to a working binary. Do not hunt through the filesystem for unrelated binaries unless the user asks.

Initialize only after the vault path is confirmed:

```bash
"$ENZYME_BIN" -p "<vault path>" init --quiet
```

Use `--use-env-llm` only when the user explicitly asks to use their own OpenAI, OpenRouter, or OpenAI-compatible provider and has set the complete provider environment. Do not inspect, print, unset, or rely on API-key environment variables during normal hosted setup.

Never delete `~/.enzyme/auth.json` or run `"$ENZYME_BIN" logout` without explicit confirmation.

If the configured path is empty, scratch, or not where the notes live, stop and ask for the correct folder before doing anything else.

## Prove It On One Real Note

Between diagnosis and the gate, you may additively create exactly one new note distilled from the user's own best existing content, if doing so would prove Enzyme's value. Lead with the user's own words and file attribution. Place two to four short real excerpts beside each other. End with one concrete next question.

This proof note edits and moves nothing. If there is not enough content for a rich proof note, produce the smallest honest starting point or skip it; never fabricate to fill it.

## One Decision

Everything before this changed nothing except any additive proof note. A no ends cleanly with the vault unchanged. Present the gate as one narrated moment the user could read aloud in under a minute:

1. **What your vault already does well**.
2. **Where Enzyme cannot leverage it yet, and why**, in plain user-visible terms.
3. **Exactly what I would move, exclude, or stamp**, with concrete paths and fields.
4. **The backup promise**, including where the backup/revert package will live for this run.
5. **The revert story**: "I will check that every affected file comes back exactly as it was."

When the plan mixes work of clearly different weight, offer at most two separately acceptable pieces, such as "just the duplicate cleanup" and "the broader re-file." A partial yes runs only the accepted piece under the same backup contract.

For the healthy-vault branch, replace the repair plan with: **"No restructuring needed; your vault already compounds well."** Show the tier map as confirmation, keep any proof note, and skip the gate.

## Restructure After Consent

Runs only after the single yes. The hard bound is moves/renames within reason plus batch frontmatter edits only. Bodies are never touched.

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

After a successful repair, refresh Enzyme and report back using the same frame you opened with: what already worked, what changed, and what small habit would help future capture.
