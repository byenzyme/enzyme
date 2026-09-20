---
name: enzyme-workspace-setup
description: Set up or check Enzyme in a Markdown workspace. Use for first setup, re-setup, diagnosis, or requested cleanup; existing Enzyme files do not prove setup is complete. Review the compiled settings, demonstrate an answer from real notes, and repair only an evidenced problem with permission. Never rewrite existing note bodies. Not for routine retrieval in a healthy workspace.
allowed-tools: Read, Glob, Grep, Bash, Write, Edit
---

# Enzyme Workspace Setup

Read `references/knowledge-practice-review.md` completely before interpreting the
notes or composing the handoff. It owns the shared judgment about the user's
practice; this skill owns Enzyme commands, permissions, changes, and recovery.
When delivered through MCP, that reference is appended inline.

## The job

Help the user answer something they care about using the notes they already have.
`compile` proposes the settings. Review that proposal, prepare search, demonstrate
one answer, and fix a problem only if the evidence warrants it. A working setup
with no note changes is a complete success. Tidiness is not a retrieval goal.

Keep the sequence explicit:

1. Check the workspace and provider; obtain permission for hosted selection.
2. Run `compile --json` once; review its saved settings with the user.
3. With setup and generation authorized, run `init`, check `doctor`, and prove an answer.
4. Offer an optional repair only for a demonstrated problem; finish with what the user can do next.

These permissions cover different effects. Reuse explicit authorization already
in the conversation; ask only for an effect or choice it has not covered. A plain
request to set up Enzyme does not authorize sending excerpts, downloading a model,
or editing notes. Never let an approval reviewer choose a provider for the user.

## Talk about the user's work

Use actual note names, subjects, and questions. Until retrieval proves an answer,
say “we can try asking,” not “this now answers.” Call the program **settings**.
Explain the mechanism once: Enzyme prepares questions from the notes to help find
related passages. Describe selected folders/tags by the questions they may help
answer, not as “anchors,” “surfaces,” “tiers,” “profiles,” or “dedicated questions.”
Do not make the user learn a glossary or another unexplained metaphor. For a date
repair, say “I'll add `date: 2026-08-26` above your writing. This edits the file's
date information; your writing stays unchanged.” Explain the backup as restoring
the file and its original date, not as frontmatter, byte identity, or checksums.

Keep routine messages small: roughly 80 words for selection consent, 150 for the
settings preview, and a two-to-four-sentence answer with a short receipt. Repairs
may need more detail for exact scope. Link settings and technical evidence instead
of narrating their contents. Numbers earn a place only when they affect a choice.

The user should be able to explain what happens next and what they are agreeing
to. Enzyme's service has a no-retention policy. Keep the normal setup conversation
focused on its purpose and changes; do not add a privacy disclaimer, speculative
retention warnings, upload-volume caveats, or an irreversible-transfer warning.
If the user asks about privacy, answer directly using that policy. Do not extend
Enzyme's policy to a separately chosen provider or the user's assistant.

A clear refusal ends that branch without another menu asking the user to reconsider.
An explicit request to undo this run's settings already authorizes that undo; do
not ask again. A simulation must label actions as planned, but missing permission
to execute real commands is not missing permission inside the fictional dialogue.

## Check the starting point

Use the supplied workspace path, or the current workspace if none was supplied.
Verify that Markdown notes live there. If it is empty, scratch, or points somewhere
else, ask for the notes folder before creating state. For SQLite sources, hand off
to `enzyme-sqlite-source`; this compile workflow supports a single Markdown vault.

```bash
ENZYME_BIN="${ENZYME_BIN:-enzyme}"
"$ENZYME_BIN" --version
"$ENZYME_BIN" -p "<vault path>" model status
```

If the CLI is missing and installation/setup was requested, inspect and use the
official CLI installer. Check chained scripts too: CLI installation must not
silently install runtime instructions, change settings, or download weights.
For diagnosis-only requests, report the missing prerequisite or request a working
`ENZYME_BIN`; do not hunt for unrelated binaries.

CLI installation, runtime instruction installation, and workspace setup are
separate operations. If this skill is loaded, runtime installation has already
succeeded; do not run `enzyme install <runtime>` unless the user explicitly asks
to install or update those instructions. Existing `.enzyme/`, `AGENTS.md`,
`CLAUDE.md`, or installed skills are evidence to inspect, not proof of health.

Record configured mode, effective mode, selected model, and network requirement
from `model status`. Preserve the configured mode when no preference was given.
`auto` uses the selected local model when installed and hosted generation otherwise.
Local generation does **not** make hosted selection local.

- Hosted generation sends selected excerpts to Enzyme's service; the index,
  embeddings, and search remain local.
- Local generation needs no network once its model is installed. Use `model list`
  to show the current download size if offering installation. Downloading a model
  or switching modes requires explicit consent; preserve `auto` unless asked to
  force a mode. Do not download or switch as a fallback after a hosted failure.
- Use `--use-env-llm` on `init` and `refresh` only when the user explicitly requests
  their own configured provider and its required environment is present. Inspect
  variable names only for that request, never secret values. Never print keys or
  change the user's shell environment/settings to make a command succeed.

For an already initialized workspace, start diagnosis with `doctor` and current
settings. Do not recompile merely to check health. If new selection is needed,
explain the proposed replacement and follow the compile step below.

## Compile one proposal

`compile` is a settings mutation and an online selection call, even when generation
will be local. Unlike init, it automatically uses inherited provider credentials
when present. For accepted Enzyme-hosted selection, use the isolated subprocess
below so an unrelated shell key cannot redirect excerpts. This leaves the parent
shell and saved provider settings alone. If the user requests their own provider
for selection, verify a compatible Decisions endpoint and use the normal command
with that explicit authorization; a chat endpoint alone is insufficient.
Before running selection, briefly explain the online setup step and the settings
it saves. Its online requirement does not depend on a local model; explain local
versus hosted preparation when reviewing settings, if relevant to the user's
choice. Keep internal routing and connection-state details out of routine copy.

> Enzyme will use its online service to suggest settings from your notes and save
> them on this machine. Your notes won't be edited. Shall I go ahead?

For re-setup, also explain that existing settings for this workspace can be
replaced. Selection permission does not authorize later generation or note edits.
There is no offline selection fallback. If unavailable, stop and report the actual
state rather than claiming nothing changed.

Before the call, keep a bounded recovery record in a unique `mktemp` directory:
workspace file hashes and mtimes, original files under the resolved Enzyme home's
`configs/`, legacy `config.toml`, and existing `.enzyme/` state. Record installed
model inventory and auth file existence/metadata without reading secrets. Default
Enzyme home is `~/.enzyme`; respect an explicit `ENZYME_HOME` in test environments.
Retain original settings bytes and mtimes so re-setup can restore them.

```bash
env -u OPENAI_API_KEY -u OPENAI_BASE_URL -u OPENAI_MODEL \
  "$ENZYME_BIN" -p "<vault path>" compile --json
```

Read the returned `program`, `saved_to`, and complete bounded `evidence`. Inspect
the file at `saved_to` too: it can contain merged settings for other workspaces.
Keep that payload and the saved file as the record of this selection. Do not re-run compile to display or edit
its result: another call can select different settings.

Check changes against the recovery record. Expected writes are the saved program
and, on first hosted use, `bootstrap.json` in Enzyme home (a reusable anonymous
machine identifier). Hosted access may also create or update a temporary credential
cache; inspect metadata only, never its secret contents. Existing program files can also be changed when compile
replaces an earlier workspace definition. Notes, index, legacy config, models,
and auth metadata must remain unchanged. Investigate any unexpected change.
Do not describe this as a read-only scan or promise “no settings changed.”

If the user declines the proposal, restore only settings changed by this run from
the saved originals; remove only newly created proposal files. Check for subsequent
edits before restoring, and stop on a conflict. Leave shared `bootstrap.json`
and service credential caches alone; mention residual service state when explaining
what was restored. Never delete
`~/.enzyme/auth.json` or run logout without explicit confirmation.

## Review what was selected

Use `evidence.candidate_entities` to compare the proposal with what selection
considered. Do not scan the vault again to reconstruct the same inventory. Read
representative supplied excerpts; inspect a specific source only when needed to
verify a consequential interpretation. Counts and folder names alone do not prove
what the user cares about.

Keep three facts distinct:

- Selected readings supply questions Enzyme uses to find related passages.
- An unselected candidate has no reading of its own; its notes may still be
  searchable or contribute through another reading. It is not an exclusion or
  a defect merely because it was not selected.
- Folder exclusions remove material from discovery/search; tag/link exclusions
  affect entity selection, not whole-note discovery. The compile evidence
  is about Markdown candidates, not a complete audit of every non-Markdown file.

Show the settings link, two or three useful choices, any consequential exclusion
or questionable interpretation, and one realistic question to test. Invite a
specific correction. Do not enumerate every candidate or force a four-tier report.
Describe pre-init search coverage as planned, not already available. If nothing
needs reorganizing, say “You can keep your notes where they are.”

Confirm the saved settings and the effective generation path before `init`.
Explain that this builds a local search database from the notes. For hosted
generation, say excerpts will be sent again to prepare questions for search.
For a local download, name its size. This is a separate effect from selection;
do not ask again if the conversation already authorized it.
Apply only accepted settings edits to the saved file, without recompiling.

## Prepare and demonstrate

```bash
"$ENZYME_BIN" -p "<vault path>" model status
# For default generation, stop if the effective path changed from consent.
# For accepted --use-env-llm, recheck that provider; model status shows defaults.
"$ENZYME_BIN" -p "<vault path>" init --quiet
"$ENZYME_BIN" -p "<vault path>" doctor
"$ENZYME_BIN" -p "<vault path>" petri --query "<one realistic user question>"
"$ENZYME_BIN" -p "<vault path>" catalyze "<question informed by relevant petri terms>"
```

Append `--use-env-llm` when the user's own provider was accepted. Use `doctor --json`
when a specific finding needs file-level evidence; save the output and extract the
relevant records instead of dumping it into conversation. Doctor needs an index;
first-setup claims about what was indexed must wait until after `init`.

Read the actual completion status and warnings. A deterministic failure needs a
concrete correction before retrying; preserve partial state and use read-only
status/doctor checks to diagnose it. Permit at most one retry for an explicitly
transient or resumable condition. Never switch providers silently. Partial success
may permit a limited retrieval demonstration, but report incomplete preparation
and do not claim all settings work or repeatedly generate to hide the warning.

Use returned source excerpts to answer the test question in two to four sentences,
with clickable citations. Relevant-sounding generated questions are not evidence
for the answer. If results are only loosely related, say what they do and do not
support and try a narrower question. Do not write a new note just to have a demo.

## Decide whether a repair helps

Ground truth is Enzyme's view, checked against the affected source. A doctor count
is a lead, not a mandate to clean. Tie each proposed repair to the question it
would help answer. Keep healthy dated folders and raw exports where they are.
The embedding cap is a product limit, not a vault defect or a reason to reorganize.

Mechanisms to check before proposing changes:

- Markdown is discoverable; a PDF, image, JSON export, or raw log alone is not.
  Do not move, convert, summarize, or rewrite raw evidence during setup.
- Recognized dates, tags, and wikilinks are usable. Plain frontmatter strings
  become link entities only through an established field named by
  `references in fields`. Map scalar/list string identity fields, never broad
  metadata such as status, type, aliases, IDs, or arbitrary objects. This does
  not create a typed people/project model.
- A folder of dated notes already preserves separate moments. A single appended
  journal needs a `log` reading to preserve its internal timeline. Structural
  recognition looks for at least three substantive date-headed entries in mostly
  chronological order (ISO, numeric, prose, or weekday-prefixed dates). Verify a
  missed or false recognition; add/remove its reading rather than split the note
  or stamp legacy `type:` metadata. A page merely listing dates is not a log.
- Missing/unreadable dates can impair time-aware retrieval; compare parsed dates
  and date sources with filenames/metadata before proposing a stamp. Doctor
  `undated_docs` means a filesystem-derived date, not an unsearchable note. File
  mtime may be the fallback date. Never invent dates from prose or infer tags/aliases.
- A split series or true duplicate may hurt retrieval, but folder scatter alone
  is not evidence. Generated/template noise can compete with authored notes;
  reference material is not noise just because it is numerous.

Prefer a settings correction (missed log, reference field, exclusion) over a note
change. Explain its effect, obtain authorization, and save the previous settings.
It does not require a note-backup ceremony. Refresh and verify the intended effect.

If changing files would materially help, offer the smallest exact scope, backed by
file-level evidence: date frontmatter, moving/renaming a split series, retiring a
verified duplicate to a reversible backup, or one existing template adjustment.
Never delete a user's note, rewrite an existing body, bulk-normalize, or create a
new taxonomy. A partial yes covers only the accepted files/actions. A refusal leaves
the working setup usable. Do not offer a broader cleanup merely to fill a stage.

Before any accepted file repair:

1. Show the exact field/value or move, the expected improvement, and a concrete
   proposed path for the retained backup and `revert.sh` before asking permission. Create a self-sufficient backup, not just git.
2. Store original bytes with relative paths in `sidecar/`; record moves, body
   hashes, settings changes, and original modification times in `manifest.json`.
   Keep a `pre.shasum` of owned notes. No body edits to existing notes are allowed;
   verify body hashes around frontmatter changes and abort on a mismatch.
3. Make `revert.sh` preview its actions, reverse moves, copy original bytes, restore
   original mtimes, and exit nonzero on a bytes/mtime mismatch. Compare only owned
   files, excluding caches/indexes and the backup itself. Never overwrite later
   user edits. Retired duplicates keep a non-Markdown suffix in the backup.
4. Test apply and revert on a copy with its own `ENZYME_HOME`, copied settings
   rebound to the copied workspace path, and no mutable paths/symlinks leading
   back to the original. Keep backups outside searchable folders or explicitly
   exclude their location so backup Markdown cannot alter the result. Establish
   an equivalent baseline first. Verify bytes,
   mtimes, and the affected doctor findings return to baseline after refresh.
   Then apply the accepted repair to the real workspace and verify it. Do not
   silently leave the user with the repair undone after testing rollback.

After settings or note repairs, run `refresh --quiet` with the accepted provider.
Quiet refresh may schedule question preparation in the background; inspect its
completion/status before claiming new questions are usable. If
`update.status == "action_required"`, inspect and perform or relay its `action`
within existing authorization. A destructive `init --force` needs its own explicit
authorization; permission for a small repair does not cover rebuilding by force. Recheck the same findings and the user's retrieval
question. Some settings changes require more work than a normal refresh; do not
claim they took effect from the file edit alone. After a real revert, refresh and
recheck too; a successful copy does not prove Enzyme's view was restored.

## Finish with a useful result

Apply **End insight-first**, **Help the user keep hold of one important thread**,
and **Choose what can keep returning** from the shared review contract. Lead with
the cited result, then one natural next question. A successful connection across
notes is not evidence that a missing tag needs fixing. Offer a capture habit only
for an observed retrieval gap, with a next-note example using an established
convention; otherwise end without one. Old notes need no cleanup first.

An optional note collecting two to four real excerpts needs separate permission
and an exact path; never fabricate content or present it as required for setup.
Likewise, one accepted change to an existing Obsidian template affects only future
notes. Core Templates/Daily Notes suffice for simple dates; no new plugin is needed,
and a template alone does not choose the note's location.

A recurring review needs agreement on task, cadence, and destination. If supported,
it must refresh, use query-aware petri and catalyze, cite sources, report insufficient
evidence, and remain read-only unless writing is separately authorized. Otherwise
leave a reusable question, not a claim that scheduling occurred.

Put the operational receipt last: what was prepared, what changed, any incomplete
work or retrieval limitation, and the settings/backup links needed to inspect or
undo it. Distinguish undoing settings from deleting the index or removing service
state; do not promise a return to the original machine state by deleting two files.
Keep technical verification and connection-state details in artifacts rather than
narrating them all.
