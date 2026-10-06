# CLI exit codes and workspace locking

Hosts that drive `enzyme` as a process (agents, Margins, scripts) can rely on
the contract below. The source of truth is `crates/enzyme-cli/src/commands/exit.rs`;
keep the two in step.

## Exit codes

| Code | Meaning | Examples |
|------|---------|----------|
| 0 | Success | Includes runs that finish with warnings: `init --quiet` reports `"status": "completed_with_warnings"` (for example a named workspace whose sources hold no documents) or `"failed"` when every catalyst generation failed but the index was built. Read `status` and `warnings` in the JSON summary. |
| 1 | Index or runtime failure | Database, embedding, network, or generator errors not classified below. |
| 2 | Usage error | Unknown flags or bad arguments (reported by the argument parser), or a `--collection` name that is a path rather than a name. |
| 3 | Configuration or program error | An `.enzyme` program or `config.toml` that does not parse or resolve; a `--collection <name>` that no program or `[workspaces.<name>]` section declares; a declared Markdown folder or SQLite database that does not exist. Checked before `init`/`refresh` touches the index. |
| 4 | Workspace busy | Another `init`/`refresh` of the same workspace (or, for `init`, a background catalyst worker) still held the lock after `--lock-timeout` seconds. Nothing was changed; retry later. |

Errors print `Error: …` on stderr. With `init --json-progress`, the last stderr
line is also a JSON event: `{"stage":"error","exit_code":N,"message":"…"}`.

Codes 5 and up are unassigned.

## Workspace lock

Foreground `init` and `refresh` take `workspace.lock` beside the index
(`$ENZYME_HOME/workspaces/<name>/` for a named workspace, `<vault>/.enzyme/` for
a path-addressed vault) for their whole run. A second caller on the same
workspace waits, then proceeds once the first finishes:

- The wait is announced on stderr: a `Waiting for …` line, or with
  `--json-progress` the event `{"stage":"lock","status":"waiting","holder_pid":…}`.
  `--quiet` waits silently.
- `--lock-timeout <SECS>` bounds the wait (default 1800). `--lock-timeout 0`
  fails at once. A timeout exits 4.

Background workers coordinate through the existing `refresh.lock`:

- A detached catalyst/init worker (`refresh` spawns one when an epoch is due)
  holds `refresh.lock` while it builds. Foreground `refresh` does not wait for
  it, so documents stay fresh during a long build.
- Foreground `init` also waits for `refresh.lock` (same timeout), because it can
  reset the database (`--force`, schema drift) and rewrites catalysts directly.
- A detached worker never starts while another holds `refresh.lock`.

## Named workspaces and configuration

`enzyme --collection <name> init|refresh` addresses a workspace by name. Its
index is always `$ENZYME_HOME/workspaces/<name>/enzyme.db`, including a workspace
whose only source is one Markdown folder. Nothing is written into the notes
folder. That folder addressed by path (`enzyme -p <folder>`) is a separate,
legacy vault with its own `<folder>/.enzyme/enzyme.db`.

Workspaces and vaults declared by an `.enzyme` program are read-only input:
automatic entity selection runs on every `init`/`refresh` and is never written
back to the program, to `config.toml`, or to a new program file. Legacy targets
configured only in `config.toml` keep their historic behavior, where the first
`init` records its automatically selected entities there.
