---
name: enzyme-workspace-setup
description: Enzyme workspace setup, re-setup, and repair. Use when the user asks to set up Enzyme, diagnose an existing setup, repair retrieval, or clean up an old setup before reinitializing. Existing `.enzyme` files or installed runtime instructions are not proof that setup is complete. Diagnoses read-only against what Enzyme actually indexed, proves value on one real note, then proposes only minimal, consented repairs. Never edits note bodies. Do not use for routine search or retrieval when the user is only asking a question of an already healthy vault.
allowed-tools: Read, Glob, Grep, Bash, Write, Edit
---

# Enzyme Workspace Setup

Before acting, read
`references/knowledge-practice-review.md` completely. It is the shared contract for
interpreting evidence, recognizing meaningful continuity, coaching missing handles,
and handing off an insight before mechanics. This skill remains authoritative for
Enzyme commands, configuration, consent, mutation, backup, revert, and proof. When
these instructions arrive through MCP, the same reference is appended inline.

The durable promise:

> You can start now. Existing files will not be changed until you say yes. If a repair is worth doing, it is small, reversible, and proved against the user's real notes.

Nothing but explicitly requested CLI installation and temporary diagnostic artifacts is written before the single consent gate. A request to "install Enzyme" authorizes the additive CLI installation, but it does not authorize changing the vault, changing Enzyme settings, downloading a local model, or sending note excerpts to a service. A "no" at the gate ends cleanly, with the vault and Enzyme settings unchanged.

The first job of everything you say is revelation, not reassurance: show the user **what Enzyme anchored on in their vault** — the high-signal folders, tags, links, and running logs where their thinking accumulates — and **what each anchor makes possible**. A little mechanism is welcome when it makes the value believable ("for each anchor it keeps standing questions drawn from your own words; those are what answer you later"). Name each anchor the proposed setup will actually use in the user's vocabulary, with its real folder name, count, or dates, and pair it with a question it newly makes answerable: "it reads `content-drafts` as a timeline now, so your June draft and this morning's entry are different moments — ask what changed between them." Group secondary evidence rather than making the consent gate unreadable. Verdicts like "healthy" or "needs repair" are the footnote to this reveal, never the headline.

Speak in terms of what the user sees in their own files, not engine internals. Engine evidence guides you; the user hears it translated. Numbers are fine, internals are not: avoid DB column names, doctor cause tokens, "dated fraction," "embedding budget," "catalyst," "indexed," or "checksum" in user-facing copy. Say what it means: "this note's date is not being read," "the engine stores this twice," "I checked every file came back exactly as it was." Before sending any user-facing message, reread it as the user: rewrite every phrase they could not say back to you in their own words.

## Inputs

The user never needs to know or name this skill. A natural request such as
"install Enzyme and set it up in this workspace" is enough to trigger the full
flow. The prompt may provide:

- A vault/workspace path, or "this workspace" to mean the current workspace root.
- Optional preferred destination for new notes.
- Optional `ENZYME_BIN`; otherwise use `enzyme` from PATH.
- Optional existing setup constraints, such as "do not edit before init" or "use my provider key."

Treat the provided path or current workspace as the workspace root unless the evidence or user says the notes live elsewhere. Verify that Markdown notes actually live there before scanning.

If the source is a SQLite database rather than a Markdown folder, use the `enzyme-sqlite-source` skill. It explores the schema read-only, obtains confirmation on a universal role binding, and preserves the resulting contract and workspace state.

Existing setup is not a skip condition. If `.enzyme/`, `AGENTS.md`, `CLAUDE.md`, `.agents/skills/`, or `.claude/skills/` already exist and the user asked for setup, re-setup, diagnosis, or repair, inspect them as evidence of the current state, then continue through the diagnosis flow. Overwrite stale Enzyme runtime instruction sections only by rerunning `enzyme install <runtime>` when the user is trying to update installed instructions; otherwise repair setup state through the diagnosis, config, init, refresh, and proof workflow below.

## Two Questions Carry The Skill

Everything below is derived from asking these two questions honestly. Prescribe yourself no procedure you cannot justify from them.

### 1. Diagnosis: What Does Enzyme Actually See?

Ground truth is Enzyme's view, not the folder's appearance. Do not tier a cluster from a file count or a folder name. A `meetings/` folder can be stale exports; a `notes/` folder can be source code.

Run `"$ENZYME_BIN" doctor` when available and account for every fact it reports before tiering anything. For every claim about what Enzyme can leverage, back it with what Enzyme actually extracted. When needed, inspect `.enzyme/enzyme.db` and compare representative files in each cluster against the indexed view, especially source path, date source, and parsed date.

A claim you can show against the doctor report or DB is a finding. A claim you can only reason toward is a hypothesis; verify it before stating it, or label it plainly as unverified.

Every gap between how the vault looks and what Enzyme sees is a finding. The mechanism facts below explain why gaps appear. They are a lens, not a checklist. If nothing rises above "leave it alone," say **"No restructuring needed; your vault already compounds well"** and stop. Never manufacture a finding to fill a template or justify a restructure.

**The normative model:** Enzyme compounds on a dated log. Notes with real dates, shared homes, and consistent entity handles stack on each other over time. A large folder of continuously dated notes is healthy infrastructure, not a mess to clean up. Content that is undated or carries metadata the parser cannot read is where compounding stalls. Duplicates waste the budget of what gets embedded. Spread across folders matters only when it splits a dated series or scatters an entity's handles apart, never as tidiness for its own sake.

Mechanism facts:

- Discovery reads Markdown. Anything saved only as PDF, transcript, export, image, JSON, or log is invisible to indexing unless there is meaningful Markdown wrapping it.
- Frontmatter can carry retrieval handles, but the mechanism is exact: title and recognized dates shape the document; tags become tag entities; literal wikilinks in any field become link entities. Plain strings such as `people: [Alice Smith]` are inert unless that vault/workspace section in `~/.enzyme/config.toml` lists the field in `frontmatter_link_fields`, for example `frontmatter_link_fields = ["people"]`. Mapped values become generic link entities, not a typed person/project model. A leading `---` proves nothing.
- Retrieval compounds when related notes share one home and dedupes by file; scattered notes cannot stack as strongly.
- Undated notes fall out of time-aware ranking, and only the most-recent slice is embedded; old or undated content can go dark.
- Frequency and recency rank entities, so a large recent generated/reference/template folder can outrank real notes. Treat authored reference material as real content unless evidence says otherwise.
- Two shapes both look like "a log" but need opposite handling. **Many dated files** — one note per day in a folder — are the healthy shape: each carries its own date and stacks over time; leave them alone. **A single file of dated entries** — a `journal.md` that grows by appending, or a generated transcript with day-by-day sections — is different: without recognition Enzyme would read it as one note on one date, collapsing its internal timeline. Enzyme auto-recognizes this shape structurally and broadly: date headers may be ISO (`2026-07-28`), numeric (`07/28/2026`), prose (`July 28, 2026`), or weekday-prefixed (`Monday, July 28, 2026`), in mostly one chronological direction, with at least three entries that have real content beneath their headers (a page that merely *lists* dates, a metrics table, or an essay citing dates does not qualify). Recognized files are read as internal timelines automatically. What recognition looks like to the user: `scan --write-config` records each recognized file as a `log:` line in their settings, next to the folders and tags — a legible ledger of what Enzyme noticed. Removing a line withdraws the file from anchor treatment; adding one promotes a log the detector missed or judged too small. Enzyme never splits or edits the file itself, and never writes recognition into the note's own metadata.

Before you read anything, snapshot the workspace to a unique temporary path with `mktemp`. Also record the existence and hashes of non-secret Enzyme state that preflight must not change: `~/.enzyme/config.toml`, the vault's `.enzyme/` state, and the installed-model inventory. For authentication state, record only file existence and metadata; never read or print its contents. After diagnosis, repeat both checks, prove byte identity, and clean up the snapshots. Diagnosis that changes the vault, config, index, or model inventory was not read-only.

Place every observed cluster in exactly one tier:

1. **Already usable**: structured Markdown, dated notes, existing wikilinks, or frontmatter fields that the active config demonstrably maps into Enzyme signal.
2. **Weakly indexable**: real content Enzyme can only partly see, such as missing dates, malformed metadata, thin bodies, or non-Markdown source.
3. **Would improve retrieval**: content that would compound much better after a small, deterministic move, exclusion, date stamp, or settings recording (a `log:` line).
4. **Can wait**: non-note structure that should not be reorganized for Enzyme, such as source code, raw exports, build outputs, and foreign-domain material.

Raw transcripts and evidence bundles stay put. Never move, edit, or summarize raw JSON, logs, transcripts, or exports during setup. Never bulk-invent `people:` or `tags:` frontmatter from body inference.

### 2. Mutation: Can Every Promise Be Proven Mechanically?

State each invariant, then prove it:

- **Read-only diagnosis**: prove zero filesystem residue with the before/after snapshot.
- **No body edits to existing notes**: capture each note body's hash before and after any frontmatter stamp and abort if it would change.
- **Full backup before first mutation**: create a self-sufficient backup of every affected file before moving it, stamping it, or changing a template. A temporary backup workspace is acceptable if it is retained through verification and the user is told where it lives; a vault-local backup is acceptable when the product surface needs a visible revert package. Do not rely solely on git.
- **Revertible**: run the revert end to end yourself and prove the notes come back byte-identical before claiming success.

Only moves/renames, date-repair frontmatter stamps, settings edits (`log:` lines, `frontmatter_link_fields`, exclusions, targets in Enzyme's config), and one small change to a template used for new notes are allowed. The user must explicitly accept every change. Each change must follow evidence already present in filenames, metadata, exact entity matches, or the user's established template conventions. Configure a frontmatter link field only when scan/file evidence shows it is an established identity/reference field with scalar or list string values; never map broad metadata such as `status`, `type`, `aliases`, IDs, or arbitrary objects. A template change may make a future date, link, or existing field easier to add; it never authorizes edits to existing note bodies. No invented aliases, bulk summaries, or inferred tags. Never write or print a secret.

Log recognition is a **settings edit, never a note edit**. When a single file of dated entries is confirmed (see the two-shapes mechanism fact), the remedy is its `log:` line in Enzyme's config: keep the line `scan --write-config` already recorded, or add one for a real recurring log the detector missed (too few entries, say). Narrate the lines at the gate in the user's terms — "Enzyme recognized these files as running logs and reads each as a timeline; here are their lines, remove any I misread" — and treat removal as a first-class correction, not a failure. Because a false recognition is one deletable line in one settings file, this is the lightest mutation in the skill; it still passes through consent because it changes what Enzyme anchors on. Do not stamp `type:` metadata into the user's files to mark logs — the engine honors such stamps as a legacy escape hatch, but recognition the user can see and curate belongs in their settings, not buried in per-file metadata. Never split or rewrite the file to normalize it — instead, if the user wants the healthier shape over time, offer "one file per day" as a future-capture habit at the close, not as a mutation now.

Never delete a user's file, not even an exact duplicate. If a duplicate must be retired, move it to a backup/retired location with a non-Markdown suffix and record the move so revert restores it.

## Connect Without Conflating Three Different Setups

Keep these operations separate:

1. **CLI installation** puts the `enzyme` binary on the machine.
2. **Runtime installation** (`enzyme install codex`, `claude`, `hermes`, or
   `openclaw`) installs agent instructions.
3. **Workspace setup** scans, configures, and initializes this vault.

If this skill is loaded, runtime installation has already succeeded. Never run
`enzyme install <runtime>` during workspace setup unless the user specifically
asked to install or update that runtime's instructions. A website quickstart may
serve a standalone user and still be wrong for the current app; do not copy its
runtime-install step blindly. In this context, the user's phrase "install Enzyme"
means install the missing CLI, not `enzyme install codex` or another runtime.

Resolve the Enzyme binary:

```bash
ENZYME_BIN="${ENZYME_BIN:-enzyme}"
"$ENZYME_BIN" --version
```

If the binary is missing and the user explicitly asked to install or set up
Enzyme, install the CLI through Enzyme's current official installation route.
Inspect the installer source, destination, and expected files before executing it;
if the route chains to another script, inspect that behavior too. If it also writes
runtime instructions, settings, or model weights, stop and disclose those extra
effects rather than executing them under CLI-only authorization. If the user asked
only to diagnose or repair, stop with the exact installation step or ask for a
working `ENZYME_BIN`. Do not hunt through the filesystem for unrelated binaries.

If the configured path is empty, scratch, or not where the notes live, stop and
ask for the correct folder before reading notes or creating state.

## Choose The Generation Path Before Any Setup Write

First inspect the actual effective behavior:

```bash
"$ENZYME_BIN" -p "<vault path>" model status
```

Record all four reported facts: configured mode, effective mode, selected model,
and whether network access is required. Do not infer them from environment
variables, an installed model file, old settings, or a website. `auto` means local
when its selected model is installed and hosted otherwise.

When the user expresses no provider or privacy preference, preserve the configured
mode; do not silently turn `auto` into forced local or forced hosted. Explain the
effective path in the single setup gate:

- **Effective local:** generation stays on the machine and requires no network.
  Do not run `model use` merely to make an already-local `auto` mode explicit.
- **Effective hosted:** selected excerpts from notes are sent to Enzyme's hosted
  generation service. The local index, embeddings, and search stay on the machine.
  State this plainly and ask for it as part of the one setup decision before init.
- **Local alternative:** run `model list` so the user sees the current download
  size and model availability. Downloading a model or switching modes requires an
  explicit yes; never do either as an automatic response to a hosted-provider
  approval failure. Prefer `model install` while preserving `auto`; use `model use`
  only when the user explicitly wants local generation to be required. Do not run
  an init or installer path that implicitly downloads model weights unless that
  download was named and accepted at the gate.
- **User-supplied provider:** use `--use-env-llm` only when the user explicitly
  asks to use their own OpenAI, OpenRouter, or OpenAI-compatible provider and the
  complete provider environment is present. Inspect names, never values. A key
  inherited by the shell is not consent to use it. Do not inspect, print, unset, or rely on API-key environment variables during normal setup.

Do not let an app-level approval reviewer make this product choice implicitly. If
it blocks hosted generation, return to the user with the hosted-transfer and local-
download choices. If it allows hosted generation, the skill's own disclosure and
consent requirement still applies.

## Diagnose Read-Only, Then Initialize After Consent

Before the gate, diagnose without writing settings:

```bash
"$ENZYME_BIN" -p "<vault path>" doctor
"$ENZYME_BIN" -p "<vault path>" scan
# Use scan output plus doctor/DB evidence to diagnose what Enzyme can see.
```

`scan --write-config` is a settings mutation. Do not run it, `init`, `refresh`, or
generation before the gate. After the user accepts the narrated setup plan and
generation path, back up every affected existing setting and run:

```bash
"$ENZYME_BIN" -p "<vault path>" scan --write-config
# Read ~/.enzyme/config.toml. This is the vault's anchor ledger: the entities
# (folders, tags, links), any configured plain frontmatter link fields, and
# recognized running logs (`log:` lines) that Enzyme will anchor on. Tune only
# the minimum entity/field/log/exclusion/target choices supported by scan/doctor
# evidence before init. If the written proposal differs materially from what the
# user accepted at the gate, stop and reconfirm instead of initializing. This is
# an exceptional scope change, not a routine second setup gate.

"$ENZYME_BIN" -p "<vault path>" model status
# Compare effective mode and Network with what the user accepted. Stop if either
# changed; auto can resolve differently if machine state changed after the gate.
"$ENZYME_BIN" -p "<vault path>" init --quiet
"$ENZYME_BIN" -p "<vault path>" petri
"$ENZYME_BIN" -p "<vault path>" petri --query "<one real setup proof prompt>"
"$ENZYME_BIN" -p "<vault path>" catalyze "<query composed from the prompt and petri vocabulary>"
```

When the user accepted BYOK, append `--use-env-llm` to `init` and later
`refresh`; never append it merely because provider variables exist.

Do not skip `scan --write-config` and config review just because `.enzyme/` already exists. Existing config and index state are evidence to inspect; they are not proof that the setup is healthy or current.

If init or generation reports a deterministic error, do not repeat the same
command unchanged or continue to another generation command. Stop the generation
flow, preserve partial state, rerun only read-only status and doctor checks, and
report exactly what remains incomplete. A second attempt requires a concrete
corrective change; never silently change providers after a failure. For an
explicitly transient or resumable condition, allow at most one bounded retry and
state the reason.

Never delete `~/.enzyme/auth.json` or run `"$ENZYME_BIN" logout` without explicit confirmation.

## Prove It On One Real Note After Consent

At the gate, you may offer exactly one additive proof note distilled from the user's own best existing content, if doing so would prove Enzyme's value. Name its exact path and create it only after the user accepts it as part of the setup plan. Lead with the user's own words and file attribution. Place two to four short real excerpts beside each other. End with one concrete next question.

This proof note edits and moves nothing. If there is not enough content for a rich proof note, produce the smallest honest starting point or skip it; never fabricate to fill it.

## Bind Capture Help To Enzyme

Apply **Help the user keep hold of one important thread** from the shared review
contract; do not create a second habit script here. Offer it only when the
diagnosis found a meaningful trail that is likely to fade.

Make Enzyme do its share. If an existing field merely needs recognition, or a real
running note needs recording in settings, include that change in the consented
setup plan. If the user already uses an Obsidian template and one small change
would remove a step from the accepted practice, show the exact change and ask
before making it. Core Obsidian Templates are enough for simple date and time
placeholders, and Daily Notes can apply a daily template; do not require a new
plugin. A template affects only notes made with it and does not choose where a note
goes.

On first setup, name the retrieval promise that the practice should make possible.
On a later setup check or repair, test that promise against naturally accumulated
notes and revise or drop the Enzyme binding when it does not hold.

## One Decision

Everything before this changed nothing except an explicitly requested CLI installation and temporary diagnostics that are removed after comparison. A no ends cleanly with the vault and Enzyme settings unchanged. Present the gate as one narrated moment the user could read aloud in under a minute, opening with the anchors reveal. The generation-path disclosure is mandatory even when secondary anchor evidence must be grouped:

1. **What Enzyme anchored on and what that buys you**: the folders, tags, links, and running logs it grabbed, each named in the user's vocabulary and paired with a question it newly makes answerable.
2. **Where Enzyme cannot leverage your vault yet, and why**, in plain user-visible terms.
3. **How the slow generation step will run**: quote the effective local/hosted/BYOK result; for hosted, say selected note excerpts leave the machine; for a proposed local install, include its current download size.
4. **Exactly what I would move, exclude, record in your settings, create as the one proof note, or change in an existing template**, with concrete paths, lines, and fields.
5. **The backup promise**, including where the backup/revert package will live for this run.
6. **The revert story**: "I will check that every affected file comes back exactly as it was."

When the plan mixes work of clearly different weight, offer at most two separately acceptable pieces, such as "just the duplicate cleanup" and "the broader re-file." A partial yes runs only the accepted piece under the same backup contract.

For the healthy-vault branch, the anchors reveal **is** the message: show what Enzyme anchored on and the value that flows through each anchor, and say **"No restructuring needed; your vault already compounds well."** Skip the repair items, but do not skip generation-path consent when initialization or networked generation is still required. A healthy vault deserves the richest reveal, not the shortest one — the user should still leave knowing exactly which of their places Enzyme is watching and what to ask it. Then use the closing guidance below.

## Apply The Accepted Setup And Repairs

Runs only after the single yes. The hard bound is moves/renames within reason, batch date-repair frontmatter edits, Enzyme settings edits (`log:` lines, `frontmatter_link_fields`, exclusions), and one explicitly accepted change to a template used for new notes. Existing note bodies are never touched, and field/log recognition never writes to note files at all.

Before the first mutation, write a backup package either to a unique temporary directory or to a visible vault-local directory selected by the product surface. It must contain:

- `sidecar/`: byte-for-byte copies of every file the repair will touch, preserving relative paths.
- `manifest.json`: every move, every frontmatter or template change, every sidecar path, and before/after body hashes for existing notes.
- `pre.shasum`: snapshot of the notes before mutation.
- `revert.sh`: a script that restores from sidecar files and reverses moves.

The revert script must:

- Print what it will do before doing it.
- Replay moves in reverse.
- Restore changed files by copying original bytes from `sidecar`, not by reconstructing diffs.
- Keep the optional proof note unless the user explicitly asked to remove it.
- Compare only the user's notes that the repair owns; exclude app-owned state such as `.enzyme/`, `.git/`, product-specific state directories, caches, and the backup package itself.
- Exit non-zero if any owned note failed to restore.

After a successful repair, refresh Enzyme. If `enzyme refresh --quiet` returns `update.status == "action_required"`, perform or relay the `action` string before continuing.

## Complete The Handoff

Apply **End insight-first** and **Choose what can keep returning** from the shared
review contract instead of maintaining Enzyme-specific closing copy. Ground the
opening insight in actual `petri` and `catalyze` source excerpts, or state plainly
when they cannot support it.

If the user accepts a recurring review and the current product surface can create
one, its task must run `enzyme refresh --quiet`, use query-aware `petri` and
`catalyze`, cite the notes that support the result, report insufficient evidence,
and remain read-only unless the user separately permits a write. Put the Enzyme
operational receipt last: what was prepared, what changed, what remains hard to
recover, and how to undo any accepted change.
