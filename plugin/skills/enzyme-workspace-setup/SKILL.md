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

## The shape of the work

`enzyme compile` chooses what to read and why, and writes a readable program.
That is the setup. Your job is what it cannot do: say what the program will and
will not reach, get consent before anything runs, prove the result on real notes,
and repair only what measured evidence justifies.

Two gates, in this order. Never merge them.

| | Gate 1 — setup | Gate 2 — repairs |
|---|---|---|
| When | after `compile`, before `init` | after `init` and `doctor` |
| Covers | the program, the generation path | changes to the user's own files |
| Evidence | `compile --json` | `doctor` |
| Touches notes | no | yes |
| Backup contract | none, and imply none | required |
| How often | always | rarely |

Nothing but explicitly requested CLI installation and temporary diagnostic artifacts is written before gate 1. A request to "install Enzyme" authorizes the additive CLI installation, but it does not authorize changing the vault, changing Enzyme settings, downloading a local model, or sending note excerpts to a service. A no at gate 1 ends cleanly with the vault and Enzyme settings unchanged. A no at gate 2 leaves a working setup from gate 1.

## Speak in the user's terms

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

Treat the provided path or current workspace as the workspace root unless the evidence or user says the notes live elsewhere. Verify that Markdown notes actually live there first.

If the source is a SQLite database rather than a Markdown folder, use the `enzyme-sqlite-source` skill. It explores the schema read-only, obtains confirmation on a universal role binding, and preserves the resulting contract and workspace state.

Existing setup is not a skip condition. If `.enzyme/`, `AGENTS.md`, `CLAUDE.md`, `.agents/skills/`, or `.claude/skills/` already exist and the user asked for setup, re-setup, diagnosis, or repair, inspect them as evidence of the current state, then continue through the flow below. They are evidence to inspect; they are not proof that the setup is healthy or current. Overwrite stale Enzyme runtime instruction sections only by rerunning `enzyme install <runtime>` when the user is trying to update installed instructions.

Keep three operations separate and never conflate them:

1. **CLI installation** puts the `enzyme` binary on the machine.
2. **Runtime installation** (`enzyme install codex|claude|hermes|openclaw`) installs agent instructions.
3. **Workspace setup** is this skill.

If this skill is loaded, runtime installation has already succeeded. Never run
`enzyme install <runtime>` during workspace setup unless the user specifically
asked to install or update that runtime's instructions. A website quickstart may
serve a standalone user and still be wrong for the current app. In this context,
"install Enzyme" means install the missing CLI.

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

## Ground truth is Enzyme's view

Do not tier a cluster from a file count or a folder name. A `meetings/` folder can be stale exports; a `notes/` folder can be source code. For every claim about what Enzyme can leverage, back it with what Enzyme actually extracted. A claim you can show against the evidence below is a finding. A claim you can only reason toward is a hypothesis; verify it before stating it, or label it plainly as unverified. Every gap between how the vault looks and what Enzyme sees is a finding — but never manufacture one to fill a template or justify a restructure. If nothing rises above "leave it alone," say so and stop.

**Re-setup is different.** When the vault is already initialized, `doctor` and `.enzyme/enzyme.db` are available from the start, so all four tiers are knowable before any gate. Compare representative files in each cluster against the indexed view, especially source path, date source, and parsed date. The staging below describes a first-time setup, where the index does not exist yet.

**The normative model:** Enzyme compounds on a dated log. Notes with real dates, shared homes, and consistent entity handles stack on each other over time. A large folder of continuously dated notes is healthy infrastructure, not a mess to clean up. Content that is undated or carries metadata the parser cannot read is where compounding stalls. Duplicates waste the budget of what gets embedded. Spread across folders matters only when it splits a dated series or scatters an entity's handles apart, never as tidiness for its own sake.

Mechanism facts. These are a lens, not a checklist:

- Discovery reads Markdown. Anything saved only as PDF, transcript, export, image, JSON, or log is invisible to indexing unless there is meaningful Markdown wrapping it.
- Frontmatter can carry retrieval handles, but the mechanism is exact: title and recognized dates shape the document; tags become tag entities; literal wikilinks in any field become link entities. Plain strings such as `people: [Alice Smith]` are inert unless that vault's program lists the field, for example `references in fields ["people"]`. Mapped values become generic link entities, not a typed person/project model. A leading `---` proves nothing.
- Retrieval compounds when related notes share one home and dedupes by file; scattered notes cannot stack as strongly.
- Undated notes fall out of time-aware ranking, and only the most-recent slice is embedded; old or undated content can go dark.
- Frequency and recency rank entities, so a large recent generated/reference/template folder can outrank real notes. Treat authored reference material as real content unless evidence says otherwise.
- Two shapes both look like "a log" but need opposite handling. **Many dated files** — one note per day in a folder — are the healthy shape: each carries its own date and stacks over time; leave them alone. **A single file of dated entries** — a `journal.md` that grows by appending, or a generated transcript with day-by-day sections — is different: without recognition Enzyme would read it as one note on one date, collapsing its internal timeline. Enzyme auto-recognizes this shape structurally and broadly: date headers may be ISO (`2026-07-28`), numeric (`07/28/2026`), prose (`July 28, 2026`), or weekday-prefixed (`Monday, July 28, 2026`), in mostly one chronological direction, with at least three entries that have real content beneath their headers (a page that merely *lists* dates, a metrics table, or an essay citing dates does not qualify). Recognized files become a `learn questions from log "<name>"` reading in the program — a legible ledger of what Enzyme noticed. Removing a reading withdraws the file from anchor treatment; adding one promotes a log the detector missed or judged too small — treat removal as a first-class correction, not a failure, since a false recognition is one deletable line. Enzyme never splits or edits the file itself. Do not stamp `type:` metadata into the user's files to mark logs: the engine honors such stamps as a legacy escape hatch, but recognition the user can see and curate belongs in their program, not buried in per-file metadata. Never split or rewrite the file to normalize it; if the user wants the healthier shape over time, offer "one file per day" as a future-capture habit at the close, not as a mutation now.

Raw transcripts and evidence bundles stay put. Never move, edit, or summarize raw JSON, logs, transcripts, or exports during setup. Never bulk-invent `people:` or `tags:` frontmatter from body inference.

## Choose the generation path

Inspect actual effective behavior before the gate:

```bash
"$ENZYME_BIN" -p "<vault path>" model status
```

Record all four reported facts: configured mode, effective mode, selected model,
and whether network access is required. Do not infer them from environment
variables, an installed model file, old settings, or a website. `auto` means local
when its selected model is installed and hosted otherwise.

When the user expresses no provider or privacy preference, preserve the configured
mode; do not silently turn `auto` into forced local or forced hosted. Explain the
effective path at gate 1:

- **Effective local:** generation stays on the machine and requires no network.
  Do not run `model use` merely to make an already-local `auto` mode explicit.
- **Effective hosted:** selected excerpts from notes are sent to Enzyme's hosted
  generation service. The local index, embeddings, and search stay on the machine.
  State this plainly and ask for it as part of gate 1.
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

## Step 1: compile, once

Before reading anything, snapshot the workspace to a unique temporary path with `mktemp`. Also record the existence and hashes of non-secret Enzyme state that preflight must not change: `~/.enzyme/configs/`, any legacy `~/.enzyme/config.toml`, the vault's `.enzyme/` state, and the installed-model inventory. For authentication state, record only file existence and metadata; never read or print its contents. Afterwards repeat both checks and clean up the temporary directory. Everything must be byte-identical except the single program file `compile` wrote under `~/.enzyme/configs/`, which is the one expected delta — name it and its hash rather than reporting "unchanged." The vault, any legacy config, the vault's `.enzyme/` state, the model inventory, and auth metadata must all be untouched. Anything else that moved means diagnosis was not read-only.

```bash
"$ENZYME_BIN" -p "<vault path>" compile --json
```

One call. It saves the program to `~/.enzyme/configs/<vault>-<hash>.enzyme` and
returns it alongside `evidence`, the inventory it selected from. It touches no
note and builds no index; backing out is deleting that one file.

`compile` is a settings mutation, so run nothing else that writes — no `init`,
no `refresh`, no generation — until gate 1 passes.

Run it **once**. Selection is a model call, so a second run re-selects and can
legitimately return different readings and profiles. Never re-run it to refresh
the program, and never narrate one run while saving another.

**Setup requires connectivity, and `compile` is itself a hosted call.** Selected
excerpts from the user's notes leave the machine during selection, before gate 1
exists. So disclose that and get a yes *before running it*:

> To propose a setup, Enzyme sends short excerpts from your notes to its hosted
> selection service. Nothing is indexed and no file changes. May I run it?

This is narrow consent for selection only. Generation is a separate disclosure at
gate 1, because the user may accept one and refuse the other. Do not fold them
together, and do not run `compile` on the assumption that a later gate will cover
it retroactively.

There is no offline path and no degraded fallback: if the call is unavailable,
stop, say setup needs a connection, and leave the vault and settings untouched.

## Step 2: tier what the payload can answer

The frame is a diff: **what the program reads, what it does not, and why.**
`evidence.candidate_entities` is the full pool selection drew from, so every
reading in the program appears in it and the leftovers are exactly what was
considered and not read. Compute it from this payload; do not walk the vault
again. (`evidence.scan_shortlist` is the narrower deterministic list, useful for
seeing what a plain scan would have picked on its own.)

**Before `init`, only two tiers are knowable:**

1. **Already usable** — a reading in the program, drawn from `candidate_entities`. Name the reading and what it now answers.
3. **Would improve retrieval** — in `candidate_entities` but not read, or read under a profile worth arguing with.

**The other two need `doctor`, which needs an index.** Do not guess them now:

2. **Weakly indexable** — read but read poorly: `undated_docs`, `frontmatter_anomalies`, thin bodies. Doctor counts these and names the files.
4. **Can wait** — invisible or out of scope: non-Markdown source, raw exports, build outputs. The pre-init evidence covers Markdown only.

This is why repairs follow `init`: the evidence justifying a repair does not exist
until the index does. "Your notes are undated" at gate 1 would be a guess wearing
the clothes of a finding.

## Gate 1: setup (always)

One narrated moment the user could read aloud in under a minute:

1. **The program**, shown and annotated. It is written to be read — show it rather than paraphrasing it into other vocabulary, which only creates drift between what you say and what was written. Annotate the handful of readings that carry the most for this user, and group the rest ("nine more folders read for what keeps returning"). A real vault produces dozens of readings and a list of over a hundred omissions; narrating each one defeats the minute.
2. **What Enzyme will not reach yet**, from tier 3, in plain terms and summarized by kind rather than enumerated — "18 running logs it noticed but did not pick up" beats eighteen paths.
3. **How generation will run**: the effective local/hosted/BYOK result; for hosted, say selected note excerpts leave the machine; for a proposed local install, its download size.

Propose changes as concrete edits to the saved program — this reading removed,
this profile changed, this log added — and apply them only after the user accepts.
Do not silently tune it on their behalf.

**If tier 3 is empty and doctor later finds nothing worth repairing, this is the
whole interaction.** Say **"No restructuring needed; your vault already compounds well"**,
init, prove it, and close. Do not open gate 2 to have something to offer. A healthy
vault still deserves the full program reveal — the user should leave knowing which
of their places Enzyme watches and what to ask it.

## Step 3: initialize and prove

The program is already on disk. Apply accepted edits by editing that file; do not
run `compile` again.

```bash
"$ENZYME_BIN" -p "<vault path>" model status
# Compare effective mode and network with what the user accepted. Stop if either
# changed; auto can resolve differently if machine state changed after the gate.
"$ENZYME_BIN" -p "<vault path>" init --quiet
# Returns a compact outcome summary. For entities and catalysts, ask petri.
"$ENZYME_BIN" -p "<vault path>" doctor
# Only works now. Source of tiers 2 and 4 and of everything gate 2 argues from:
# undated_docs and frontmatter_anomalies name their files.
"$ENZYME_BIN" -p "<vault path>" petri --query "<one real setup proof prompt>"
# Query-scoped on purpose. A bare petri returns every top entity with its children.
"$ENZYME_BIN" -p "<vault path>" catalyze "<query composed from the prompt and petri vocabulary>"
```

When the user accepted BYOK, append `--use-env-llm` to `init` and later `refresh`;
never append it merely because provider variables exist.

If init or generation reports a deterministic error, do not repeat the same
command unchanged or continue to another generation command. Stop, preserve
partial state, rerun only read-only status and doctor checks, and report exactly
what remains incomplete. A second attempt requires a concrete corrective change;
never silently change providers after a failure. For an explicitly transient or
resumable condition, allow at most one bounded retry and state the reason.

Never delete `~/.enzyme/auth.json` or run `"$ENZYME_BIN" logout` without explicit confirmation.

## Gate 2: repairs, after `init` (only when notes would change)

Open only when doctor produced a concrete finding worth acting on. Quote the
counts. This is where the backup and revert contract lives, because this is the
only place the user's own files move:

1. **Exactly what I would move, exclude, record in your program, create as the one proof note, or change in an existing template**, with concrete paths, lines, and fields, each traced to the doctor fact that justifies it.
2. **The backup promise**, including where the backup/revert package will live for this run.
3. **The revert story**: "I will check that every affected file comes back exactly as it was."

When the plan mixes work of clearly different weight, offer at most two separately
acceptable pieces, such as "just the duplicate cleanup" and "the broader re-file."
A partial yes runs only the accepted piece under the same backup contract.

At this gate you may also offer exactly one additive proof note distilled from the user's own best existing content, if it would prove Enzyme's value. Name its exact path and create it only after acceptance. Lead with the user's own words and file attribution, place two to four short real excerpts beside each other, and end with one concrete next question. It edits and moves nothing. If there is not enough content, produce the smallest honest starting point or skip it; never fabricate to fill it.

### What a repair is for

Each closes a gap doctor measured, by a named mechanism:

- **Date-repair frontmatter stamps** return `undated_docs` to time-aware ranking and embedding, which is where undated notes go dark.
- **Moves/renames** put a split dated series back under one home so it stacks.
- **Program edits** — a `log` reading for a running log the selection missed, `references in fields` for a real identity field, an exclusion for material that should not compete — change what Enzyme anchors on without touching a note. These are the cheapest repairs and usually the first to reach for.
- **One template change** removes a step from the user's capture habit so the next note arrives legible. It affects only notes made with it and never authorizes edits to existing bodies.

The hard bound is exactly those four. Existing note bodies are never touched, and
field/log recognition never writes to note files at all. Each change must follow
evidence already present in filenames, metadata, exact entity matches, or the
user's established template conventions. Configure a reference field only when
evidence shows an established identity/reference field with scalar or list string
values; never map broad metadata such as `status`, `type`, `aliases`, IDs, or
arbitrary objects. No invented aliases, bulk summaries, or inferred tags. Never
write or print a secret.

Never delete a user's file, not even an exact duplicate. If a duplicate must be
retired, move it to a backup/retired location with a non-Markdown suffix and
record the move so revert restores it.

### Prove every promise mechanically

- **Read-only diagnosis**: prove zero filesystem residue with the before/after snapshot.
- **No body edits to existing notes**: capture each note body's hash before and after any frontmatter stamp and abort if it would change.
- **Full backup before first mutation**: a self-sufficient backup of every affected file before moving it, stamping it, or changing a template. A temporary backup workspace is acceptable if retained through verification and the user is told where it lives; a vault-local backup is acceptable when the product surface needs a visible revert package. Do not rely solely on git.
- **Revertible**: run the revert end to end yourself and prove the notes come back byte-identical *and with their original modification times* before claiming success.

**Bytes alone are not a restore.** An undated note takes its date from the
filesystem, so a file restored with a fresh mtime is byte-identical and still
wrong: Enzyme now reads it as recent, and it keeps an embedding slot it had
previously lost. Record mtimes in the manifest, restore them in `revert.sh`, and
verify them alongside the hashes. A revert that reports success while `doctor`
counts stay moved has not reverted.

The backup package must contain:

- `sidecar/`: byte-for-byte copies of every file the repair will touch, preserving relative paths.
- `manifest.json`: every move, every frontmatter or template change, every sidecar path, before/after body hashes for existing notes, **and each file's original modification time**.
- `pre.shasum`: snapshot of the notes before mutation.
- `revert.sh`: restores from sidecar files and reverses moves.

The revert script must print what it will do before doing it, replay moves in
reverse, restore changed files by copying original bytes from `sidecar` rather
than reconstructing diffs, restore each file's recorded modification time, keep
the optional proof note unless the user asked to remove it, compare only the
user's notes that the repair owns (excluding `.enzyme/`, `.git/`, product state,
caches, and the backup package itself), and exit non-zero if any owned note
failed to restore in bytes or mtime.

After a successful repair, refresh so it reaches the index — a date stamp or a new
reading changes nothing until it is re-read. If `enzyme refresh --quiet` returns
`update.status == "action_required"`, perform or relay the `action` string. Then
rerun `doctor` and show the same counts you quoted at the gate, so the user sees
the repair land rather than taking it on trust.

After a revert, refresh and rerun `doctor` too. The counts must return to their
pre-repair values. If they do not, the revert was incomplete no matter what the
file comparison said — say so plainly and report what is still changed.

## Bind Capture Help To Enzyme

Apply **Help the user keep hold of one important thread** from the shared review
contract; do not create a second habit script here. Offer it only when the
diagnosis found a meaningful trail that is likely to fade.

Make Enzyme do its share. If an existing field merely needs recognition, or a real
running note needs a reading, include that change in the consented plan. If the
user already uses an Obsidian template and one small change would remove a step
from the accepted practice, show the exact change and ask before making it. Core
Obsidian Templates are enough for simple date and time placeholders, and Daily
Notes can apply a daily template; do not require a new plugin. A template affects
only notes made with it and does not choose where a note goes.

On first setup, name the retrieval promise that the practice should make possible.
On a later setup check or repair, test that promise against naturally accumulated
notes and revise or drop the Enzyme binding when it does not hold.

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
