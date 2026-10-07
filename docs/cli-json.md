# CLI JSON surfaces

Machine-readable output of the `enzyme` CLI for hosts and agents. With `--json`, a
command writes exactly one JSON document to stdout, whether or not stdout is a
terminal. Errors go to stderr with a non-zero exit code.

Every envelope names its shape in a `schema` field. A version suffix (`.v1`) changes
only when a field is removed or changes meaning; new fields can appear in any version,
so parsers must ignore keys they don't know.

Address a named workspace with `--workspace <name>`. `--collection <name>` is an
accepted alias.

| Command | Schema |
|---|---|
| `enzyme search <query> --json` | `enzyme.search.v1` |
| `enzyme status --json` | `enzyme.status.v1` |
| `enzyme spec profiles --json` | `enzyme.profiles.v1` |
| `enzyme spec plan [<file>] --json` | `enzyme.spec-plan.v1` |
| `enzyme model list --json` | `enzyme.models.v1` |
| `enzyme scan [--json]` | `scan.v2` (in `schema_version`, see below) |
| `enzyme catalyze <query> --json` | unversioned legacy envelope (see below) |

## Choosing a generator: `--llm`

`init` and `refresh` take `--llm <mode>`. These are the only commands that generate
catalysts; `catalyze`, `search`, and `status` never do.

| Mode | Generator |
|---|---|
| *(omitted)* | Today's default: the persisted `enzyme model` preference (an installed local model when selected or available), otherwise Enzyme hosted. |
| `env` | An OpenAI-compatible endpoint from the environment only: `OPENAI_API_KEY` (required), `OPENAI_BASE_URL` (default `https://api.openai.com/v1`), `OPENAI_MODEL` (default `gpt-4o-mini`). Never reads stored Enzyme auth, fetches hosted config, deducts credits, or touches the hosted lease cache. Fails before any work if `OPENAI_API_KEY` is unset. |
| `local` | The selected local model in `$ENZYME_HOME/models`. Fails before any work if it is not installed (or the binary lacks local-model support). Never falls back to env or hosted. |
| `hosted` | Enzyme hosted generation, ignoring `OPENAI_*` and installed local models. |
| `none` | Index, embed, and select only. No catalyst generation, no generator resolution, no network. `refresh` schedules no catalyst epoch. |

With `env`, `local`, or `none`, `refresh --quiet` also skips the background binary
update check, so the only network these modes use is `OPENAI_BASE_URL` (for `env`).
Hosted generation caches its short-lived lease at `$ENZYME_HOME/api-cache`
(`ENZYME_API_CACHE_PATH` overrides), never in a directory shared between homes.

`--use-env-llm` is an alias for `--llm env` and cannot be combined with `--llm`.
Workers that `init`/`refresh` spawn in the background inherit the chosen mode.

## `enzyme.search.v1`

`enzyme search <query> [-n <limit>] [--phrase <text>] --json`

Opens the index read-only (never creates, migrates, or writes it). Runs three searches
and returns each list separately so the host decides how to merge them. A missing
index is an error (exit 3) that tells the caller to run `enzyme init`; so is an index
built by an older binary (exit 3), until `init` or `refresh` rebuilds it.

```json
{
  "schema": "enzyme.search.v1",
  "query": "antenna mounting decision",
  "phrase": "phased array kit",
  "limit": 10,
  "bridged": "ready",
  "document_count": 42,
  "catalyst_hits": [
    {
      "path": "projects/heliograph-antenna.md",
      "score": 1.218,
      "content": "…chunk text…",
      "via_catalyst_id": "1907b9e7-…",
      "via_catalyst_text": "What does the antenna mounting decision trade away…"
    }
  ],
  "direct_hits": [ { "path": "…", "score": 0.61, "content": "…", "via_catalyst_id": null, "via_catalyst_text": null } ],
  "exact_hits":  [ { "path": "…", "score": 1.0,  "content": "…", "via_catalyst_id": null, "via_catalyst_text": null } ],
  "top_catalysts": [
    { "id": "1907b9e7-…", "text": "…", "entity": "projects", "topic_name": "tradeoffs",
      "relevance_score": 0.71, "contribution_count": 3 }
  ]
}
```

- `limit` (`-n`, default 10) caps each hit list separately.
- `bridged`: `ready` when the index has catalysts; otherwise `no_generator`, and
  `catalyst_hits` and `top_catalysts` are empty.
- `catalyst_hits`: catalyst-bridged search. `via_catalyst_*` is the bridge that
  contributed the strongest score.
- `direct_hits`: cosine similarity between the query and every embedded chunk.
- `exact_hits`: case-insensitive literal match of `phrase` (defaults to the query)
  within indexed chunks, newest documents first; `score` is always `1.0`. A blank
  phrase returns no hits. Hosts that only want literal lookup for distinctive phrases
  should apply that policy themselves.
- `path` is the document reference: vault-relative for Markdown,
  `sqlite:<source>/…` for SQLite sources.

## `enzyme.status.v1`

`enzyme status --json`

Includes every field of the MCP `status` tool, plus `schema`, `initialized`,
`selection`, and `index`. Status never creates, migrates, or writes the index: it
opens it read-only. An uninitialized vault or workspace still exits 0:

```json
{ "schema": "enzyme.status.v1", "initialized": false, "workspace": "notes",
  "vault": "…", "database": "…/enzyme.db" }
```

An index built by an older binary (missing tables or migrated columns) is reported
without reading further, and without migrating it; the next `init` or `refresh`
rebuilds it:

```json
{ "schema": "enzyme.status.v1", "initialized": true, "schema_outdated": true,
  "workspace": "notes", "vault": "…", "database": "…/enzyme.db",
  "update": { "status": "schema_outdated", "regen_required": true,
              "reason": "search index schema is not compatible: missing column chunks.entry_date",
              "binary": { "current": "0.11.1", "last_outcome": null } } }
```

Initialized:

```json
{
  "schema": "enzyme.status.v1",
  "initialized": true,
  "workspace": "notes",
  "vault": "…",
  "database": "…/enzyme.db",
  "schema_outdated": false,
  "documents": 42, "embedded": 42, "awaiting_embedding": 0,
  "entities": 12, "catalysts": 35,
  "catalyst_mode_configured": "auto", "catalyst_mode": "hosted",
  "catalyst_model": "catalyst-135m-v3", "catalyst_model_installed": false,
  "api_key_configured": false, "api_access": "enzyme-managed",
  "model": "ese (compiled-in, dim=512)",
  "last_background_refresh": null,
  "sources": [
    { "name": "messages", "database": "/…/messages.db", "documents": 7,
      "row_count": 7, "bucket_count": 1, "last_refresh_ms": 1700000000000,
      "stale": false, "reachable": true }
  ],
  "markdown": { "files_discovered": 35, "files_new": 1, "files_modified": 0,
                "files_deleted": 0, "stale": true },
  "selection": {
    "selected": 5, "ready": 3, "pending": 1, "skipped": 1, "unchecked": 0,
    "automatic": 2,
    "entities": [
      { "name": "projects", "type": "folder", "state": "ready", "origin": "reading",
        "catalysts": 7 },
      { "name": "radio", "type": "tag", "state": "pending", "origin": "automatic",
        "catalysts": 0 },
      { "name": "Marisol", "type": "link", "state": "skipped", "origin": "automatic",
        "catalysts": 0,
        "skip_reason": { "kind": "thin_context", "documents": 1, "tokens": 40,
                         "min_documents": 2, "min_tokens": 200 } }
    ]
  },
  "index": {
    "built_by": "0.11.1",
    "schema_fingerprint": "98740a1e…",
    "cache_epoch": 3,
    "epoch": {
      "active_epoch_id": "…", "active_epoch_completed_at_ms": 1700000000000,
      "active_epoch_corpus_generation": 4, "corpus_generation": 4,
      "corpus_changed_since_epoch": false, "build_epoch_id": null,
      "contract_fingerprint": "…"
    }
  },
  "update": { "status": "up_to_date", "regen_required": false, "reason": null,
              "binary": { "current": "0.11.1", "last_outcome": null } }
}
```

- `sources[]`: one entry per SQLite source. `stale`/`stale_reason` compare the
  source database with the last indexed refresh. `reachable`/`reachability_error`
  report whether the source opens read-only now.
- `markdown`: Markdown freshness, `null` for a named workspace without a Markdown
  source. `files_new` are on disk but not indexed, `files_modified` changed on disk
  (by mtime) after they were indexed, `files_deleted` are indexed but gone or now
  excluded; `stale` is true when any is non-zero. An edit that keeps an older mtime
  is not detected here (`refresh` still hashes every file).
- `selection.entities[]`: the entities selection would choose now, computed with the
  same selection and job construction `init` uses, without calling a model. To keep
  status cheap, at most 16 entities without catalysts get a job built; any others
  report `unchecked`.
  - `ready`: has catalysts.
  - `pending`: a generation job exists, but there are no catalysts yet (not
    generated, generation failed, or the last run used `--llm none`).
  - `skipped`: generation would produce no job. `skip_reason.kind` is one of
    `no_occurrences`, `no_evidence_before_reference` (`occurrences`),
    `no_usable_evidence` (`occurrences`), or `thin_context` (`documents`, `tokens`,
    `min_documents`, `min_tokens`).
  - `unchecked`: no catalysts, and beyond the 16-entity planning cap.
  - `origin` (added within v1): `reading` when a declared reading selected the
    entity, `automatic` when automatic selection did (every entity of a vault
    without readings; additions from `learn questions automatically`).
    `selection.automatic` counts the `automatic` entries.
- `index.built_by`: binary version that last wrote the index. `cache_epoch` increases
  whenever collection-derived caches should be invalidated. `epoch` describes the
  active catalyst epoch, and `contract_fingerprint` identifies the generation
  contract it was built under.

The text output of `enzyme status` adds a `Markdown: stale (…)` line when Markdown
files changed since the last index, and an outdated-schema notice.

## `enzyme.spec-plan.v1`

`enzyme spec plan <file> --json` previews selection and generation jobs for each
vault or workspace the file declares, without model calls or writes.

```json
{
  "schema": "enzyme.spec-plan.v1",
  "vaults": [
    { "vault": "…", "database": "…", "reference": { "instant": "…", "ms": 0, "source": "wall clock" },
      "prompts": "hosted",
      "selection": "readings and automatic",
      "readings": [ { "source": "folder \"meetings\"", "jobs": [ … ], "notes": [ … ] } ],
      "automatic": { "limit": 20, "declared": 2, "up_to": null, "slots": 18,
                     "picked": [ { "name": "research", "entity_type": "folder" } ],
                     "expanded": [] },
      "other_jobs": [ … ], "other_skipped": [ … ],
      "totals": { "jobs": 9, "requested_questions": 80, "excerpts": 40, "skipped": 1 } }
  ]
}
```

- `selection`: `readings`, `automatic` (no readings), or `readings and automatic`
  (`learn questions automatically`). `automatic` is present only with that statement.
- Jobs under a reading came from that reading; `other_jobs`/`other_skipped` are
  automatic selection's. The language reference (`tools/reading-spec/README.md`)
  describes every field.

### Named workspaces and the explain view

`enzyme --workspace <name> spec plan --json` plans a named workspace's program (the
`$ENZYME_HOME/configs/*.enzyme` file that declares it) against its index in
`$ENZYME_HOME/workspaces/<name>/`. Pass a file as well to preview a desired program's
declaration of that workspace against the same index. Like the path form it opens
the index read-only and makes no model calls, embeddings or writes (SQLite may
create its empty `-wal`/`-shm` reader files beside the index). A workspace no
program declares, or one not yet indexed, exits 3; see `docs/cli-exit-codes.md`.

Every plan, by workspace or by file, carries the explain view. Indexes in `entities`
and `skipped` point into `readings`.

```json
{
  "workspace": "work",
  "entities": [
    { "kind": "tag", "name": "focus", "origin": "reading", "reading": 0,
      "status": "planned", "questions": 3 },
    { "kind": "link", "name": "alice", "origin": "expanded", "reading": 1,
      "expanded_from": "people", "status": "planned", "questions": 3 },
    { "kind": "tag", "name": "projects", "origin": "pattern", "reading": 2,
      "status": "skipped", "questions": 0,
      "skip": { "code": "thin_context",
                "reason": "too little material: 1 note(s), 30 tokens; needs at least 1 and 32" } }
  ],
  "skipped": [
    { "scope": "reading", "code": "not_indexed", "reason": "not indexed yet: no indexed note has it",
      "reading": 3, "kind": "tag", "name": "absent", "count": 1 },
    { "scope": "entity", "code": "thin_context", "reason": "…", "reading": 2,
      "kind": "tag", "name": "projects", "count": 1 },
    { "scope": "candidates", "code": "below_link_frequency",
      "reason": "4 linked page(s) are linked fewer than 3 times",
      "reading": 1, "kind": null, "name": null, "count": 4 }
  ],
  "counts": { "selected": 3, "planned": 2, "skipped": 1,
              "by_origin": { "expanded": 1, "pattern": 1, "reading": 1 },
              "candidates_skipped": 5 },
  "truncated": { "limit": 20, "entities": 13, "skipped": 0, "jobs": 9, "job_skips": 0 }
}
```

- `entities[]`: every selected entity, in selection order. `kind` is `folder`, `tag`,
  `link`, `log` or `collection`. `origin` is `reading` (a declared reading names it),
  `pattern` (a `tags|links matching` reading matched it), `who_link` (an
  `including who links` expansion), `expanded` (a linked page of the folder in
  `expanded_from`), or `automatic`. `reading` is null for automatic picks and their
  expansions. `status` is `planned` (gets a generation job asking `questions`) or
  `skipped`, with `skip.code` and `skip.reason`.
- `skipped[]`: what the program names or could reach but will not learn about,
  ordered by `scope`: `reading` (the declared entity itself), `entity` (selected, but
  no job; repeats `entities[].skip`), `candidates` (`count` unselected matches or
  linked pages, unnamed), and `automatic`. `reason` is a short sentence for people;
  match on `code`:

  | `code` | Scope | Meaning |
  |---|---|---|
  | `not_indexed` | reading | No indexed note has the entity yet |
  | `left_out` | reading, candidates | Removed by `leave out` or a built-in exclusion |
  | `no_linked_pages` | reading | `including linked pages`, but no page in the folder is linked |
  | `no_matches` | reading | A pattern or who-link expansion matched nothing |
  | `below_link_frequency` | candidates | Linked pages linked fewer than 3 times |
  | `inactive` | candidates | Linked pages with no activity in the year before the reference date |
  | `below_density_gate` | candidates | A plain folder reading did not expand: too few linked pages for its size |
  | `own_reading` | candidates | Covered by their own reading instead |
  | `cap_reached` | candidates, automatic | More qualified than the reading's limit, or readings fill every automatic slot |
  | `no_dated_evidence` | candidates, entity | No notes dated at or before the reference instant |
  | `no_occurrences` | entity | Not mentioned in any indexed note |
  | `no_usable_evidence` | entity | Mentions yielded no usable excerpt |
  | `thin_context` | entity | Too few notes or tokens to ask good questions |

  New codes may appear; treat an unknown code by showing `reason`.
- `counts`: always describes the whole plan. `planned` equals `totals.jobs`;
  `candidates_skipped` sums `count` over non-`entity` skips.
- `--limit N` (bounded mode) lists at most N entries in each entity list: `entities`,
  `skipped`, `other_jobs`, `other_skipped`, and each reading's `jobs` and `skipped`.
  `counts` and `totals` stay complete, and `truncated` (present only when something
  was cut) says how many entries each kind of list lost. Selection itself is bounded
  (at most 20 automatic picks, plus each expanded folder's limit), so the work grows
  with evidence per selected entity rather than entity count: a full plan took about
  0.9 s warm (1.5 s cold) on a 7,151-document index.

## `enzyme.profiles.v1`

`enzyme spec profiles --json`

```json
{
  "schema": "enzyme.profiles.v1",
  "profiles": [
    { "key": "tension_trace", "display_name": "Tension Trace",
      "description": "…user-facing explanation…", "default": true,
      "definition": { "seek": "…", "notice": ["…"], "ask": ["…"], "note": null, "recognize": ["…"] } }
  ],
  "formats": [
    { "key": "question", "display_name": "Questions", "description": "…", "default": true }
  ]
}
```

`definition` holds the fields of the readable `profile` block that
`enzyme spec profiles` prints as program text.

## `enzyme.models.v1`

`enzyme model list --json`

```json
{
  "schema": "enzyme.models.v1",
  "mode": "auto",
  "selected": "catalyst-135m-v3",
  "active": null,
  "models_dir": "/…/.enzyme/models",
  "models": [
    { "name": "catalyst-135m-v3", "installed": false, "active": false, "path": null,
      "size_bytes": 91727072, "registry": true,
      "url": "https://github.com/…/enzyme-catalyst-135m-v3.gguf",
      "sha256": "710a79ee…", "context_window": 8192,
      "description": "SmolLM2-135M catalyst fine-tune …" }
  ]
}
```

Installed models come first. `size_bytes` is the file size when installed, otherwise
the registry size. Models installed with `--url` have `registry: false` and null
registry fields. `active` is the model `--llm local` (and the default mode) would use.

## Existing envelopes

- `enzyme scan` always prints JSON. Its shape is named by
  `"schema_version": "scan.v2"`, not `schema`. `--json` is accepted for symmetry.
  Use `enzyme scan --workspace <name>` for a named workspace.
- `enzyme catalyze <query>` prints the legacy catalyze envelope (`results[]` with
  `file_path`/`similarity`/`content`/`via_catalyst_*`, `top_contributing_catalysts`,
  `query`, `search_strategy`, `processing_time`, `total_results`). Without `--json` it
  draws a text tree when stdout is a terminal. With `--json` it always prints JSON.
  Before the index exists it prints the envelope with an `error` field and exits 3.
  New integrations should use `search --json`.
- `enzyme init --quiet` / `--json-progress`, `enzyme refresh --quiet`, and
  `enzyme doctor --json` keep their existing unversioned shapes.
