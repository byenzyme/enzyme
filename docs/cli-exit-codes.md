# CLI exit codes and workspace locking

Hosts that drive `enzyme` as a process (agents, Margins, scripts) can rely on
the contract below. The source of truth is `crates/enzyme-cli/src/commands/exit.rs`;
keep the two in step.

## Exit codes

| Code | Meaning | Examples |
|------|---------|----------|
| 0 | Success | Includes runs that finish with warnings: `init --quiet` reports `"status": "completed_with_warnings"` (for example a named workspace whose sources hold no documents, or some entities whose catalyst generation failed). Read `status` and `warnings` in the JSON summary. |
| 1 | Index or runtime failure | Database, embedding, network, or generator errors not classified below. |
| 2 | Usage error | Unknown flags or bad arguments (reported by the argument parser); a `--workspace` name that is a path rather than a name; `--workspace` given to `spec` or Markdown `compile`, or `--vault` to `compile --preset`. |
| 3 | Configuration or program error | An `.enzyme` program or `config.toml` that does not parse or resolve; a `--workspace <name>` that no program or `[workspaces.<name>]` section declares; a declared Markdown folder or SQLite database that does not exist (checked before `init`/`refresh` touches the index); `--llm env` without `OPENAI_API_KEY` or `--llm local` without an installed model; `search` or `catalyze` before the index exists (run `enzyme init`); `search` against an index built by an older binary (run `enzyme refresh` or `init`). |
| 4 | Workspace busy | Another `init`/`refresh` of the same workspace (or, for `init`, a background catalyst worker) still held the lock after `--lock-timeout` seconds. Nothing was changed; retry later. |
| 5 | Catalysts failed | `init` built and embedded the index (so `search` works), but catalyst generation failed its quality gate (no catalysts, or fewer than half the entities succeeded). `init --quiet`/`--json-progress` still print the summary on stdout, with `"status": "failed"`. |

Errors print `Error: …` on stderr. With `init --json-progress`, the last stderr
line is also a JSON event: `{"stage":"error","exit_code":N,"message":"…"}`.

JSON error envelopes cover `init` (the `--json-progress` error event, and the
stdout summary for exit 5) and `workspace plan|apply|recover --json`, which print
`{"ok":false,"error":{"kind":"stale|altered|unsupported|unrecoverable|invalid","message":…}}`
on stdout (exit 1; an invalid workspace name exits 2). `catalyze` also prints its
legacy envelope with an `error` field when the index is missing (exit 3). Other `--json` commands
(`search`, `status`, `spec profiles`, `model list`, `scan`) report errors as
`Error: …` text on stderr with the exit codes above; their stdout is empty.

Codes 6 and up are unassigned.

## Workspace lock

Foreground `init` and `refresh` take `workspace.lock` beside the index
(`$ENZYME_HOME/workspaces/<name>/` for a named workspace, `<vault>/.enzyme/` for
a path-addressed vault) for their whole run. A second caller on the same
workspace waits, then proceeds once the first finishes:

- The wait is announced on stderr: a `Waiting for …` line, or with
  `--json-progress` the event `{"stage":"lock","status":"waiting","holder_pid":…}`.
  `--quiet` waits silently.
- `--lock-timeout <SECS>` bounds the wait (default 1800; 5 for
  `refresh --quiet`, the hook hot path). `--lock-timeout 0` fails at once. A
  timeout exits 4. The same bound applies to `init`'s wait for a background
  catalyst worker.
- `init` through the MCP server takes the same lock and waits at most 10s.

Background workers coordinate through the existing `refresh.lock`:

- A detached catalyst/init worker (`refresh` spawns one when an epoch is due)
  holds `refresh.lock` while it builds. Foreground `refresh` does not wait for
  it, so documents stay fresh during a long build.
- Foreground `init` also waits for `refresh.lock` (same timeout), because it can
  reset the database (`--force`, schema drift) and rewrites catalysts directly.
- A detached worker never starts while another holds `refresh.lock`.
- The background update check (spawned only by default/hosted `refresh
  --quiet`, after its local writes) records its outcome in the index only under
  `workspace.lock`; if a foreground run holds it for more than 5s it skips.

## Named workspaces and configuration

`enzyme --workspace <name> init|refresh` (alias `--collection`) addresses a workspace by name. Its
index is always `$ENZYME_HOME/workspaces/<name>/enzyme.db`, including a workspace
whose only source is one Markdown folder. Nothing is written into the notes
folder. That folder addressed by path (`enzyme -p <folder>`) is a separate,
legacy vault with its own `<folder>/.enzyme/enzyme.db`.

Switching a host from path addressing to `--workspace` therefore builds a
second index: the first named `init` re-indexes and re-embeds the folder and
generates catalysts again (hosted generation spends credits). The old
`<folder>/.enzyme` index is not reused, migrated, or updated afterwards; delete
it once nothing addresses the folder by path.

Workspaces and vaults declared by an `.enzyme` program are read-only input:
automatic entity selection runs on every `init`/`refresh` and is never written
back to the program, to `config.toml`, or to a new program file. Legacy targets
configured only in `config.toml` keep their historic behavior, where the first
`init` records its automatically selected entities there.
