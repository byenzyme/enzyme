---
name: enzyme-sqlite-source
description: Explore an unfamiliar SQLite database, bind records to Enzyme's universal entity/timestamp/content/collection roles, show evidence for confirmation, freeze a deterministic per-workspace TOML source contract, and verify native init/refresh results. Use for email, message, CRM, archive, or other structured SQLite sources with no app-specific preset. Never modify source rows.
---

# Enzyme SQLite Source

The durable promise:

> Explore read-only. Show joined records and timestamp interpretation before writing anything. Once confirmed, freeze every judgment into a contract that `enzyme init` and `enzyme refresh` execute without an LLM.

The conceptual roles and TOML keys are:

- `entity` / `who`: repeated categorical values naming who or what.
- `timestamp` / `when`: when the record happened.
- `content` / `what`: the record body.
- `collection` / `where`: optional thread, mailbox, folder, or comparable grouping.

Never edit, migrate, vacuum, attach to, or create objects in the source database. Open it read-only. Do not write config, workspace files, or Enzyme data before the evidence gate.

## Inputs and workspace shapes

The path keyed by `[vaults."path"]` is the **workspace home**: the directory holding `.enzyme` state, indexed documents, embeddings, catalysts, and native source sync state. It need not contain Markdown.

Every reference to the global `~/.enzyme` home in this skill resolves to `$ENZYME_HOME` when that environment variable is set. In particular, the global config is `$ENZYME_HOME/config.toml`; otherwise it is `~/.enzyme/config.toml`. This does not change the workspace-local `$WORKSPACE_HOME/.enzyme` state directory.

Support both first-class shapes:

1. **Existing Markdown workspace:** add a `sources.<name>` tributary beside its existing entity, log, and exclusion settings. Native source execution must preserve both Markdown and database documents.
2. **Standalone SQLite workspace:** after confirmation, create or use an empty directory, add a vault section containing only `sources.<name>`, then initialize with zero Markdown files.

Resolve absolute paths and tools:

```bash
SOURCE_DB="/absolute/path/to/source.sqlite"
WORKSPACE_HOME="/absolute/path/to/workspace-home"
ENZYME_BIN=enzyme
test -f "$SOURCE_DB"
sqlite3 -readonly "$SOURCE_DB" 'PRAGMA query_only; PRAGMA quick_check;'
"$ENZYME_BIN" --version
```

Before consent, inspect an existing workspace without changing it:

```bash
test ! -e "$WORKSPACE_HOME" || test -d "$WORKSPACE_HOME"
test ! -d "$WORKSPACE_HOME" || find "$WORKSPACE_HOME" -type f -name '*.md' -not -path '*/.enzyme/*' -print
```

For standalone onboarding, create a missing directory only after confirmation. For a live database, use an application-safe snapshot or SQLite's backup API; do not copy a lone main file while a WAL may contain committed data.

## 1. Explore read-only

Evidence comes from values and joins, not names such as `date`, `from_id`, or `text`.

### Survey schema and volume

```bash
sqlite3 -readonly "$SOURCE_DB" <<'SQL'
.headers on
.mode column
SELECT name, type
FROM sqlite_schema
WHERE type IN ('table','view','trigger') AND name NOT LIKE 'sqlite_%'
ORDER BY type, name;
SELECT type, name, tbl_name, sql
FROM sqlite_schema
WHERE type IN ('table','view','trigger') AND name NOT LIKE 'sqlite_%'
ORDER BY type, name;
PRAGMA foreign_key_list('candidate_table');
PRAGMA index_list('candidate_table');
PRAGMA table_info('candidate_table');
SQL
sqlite3 -readonly "$SOURCE_DB" 'SELECT count(*) AS rows FROM "candidate_table";'
```

Inspect views and triggers too. A view may encode canonical joins; triggers may explain timestamps or soft deletion. Many application databases omit declared foreign keys.

### Find entity candidates

For every text or foreign-key candidate, measure coverage, cardinality, and common values:

```sql
SELECT count(*) AS rows,
       count(candidate) AS nonnull,
       count(DISTINCT candidate) AS distinct_values,
       round(1.0 * count(DISTINCT candidate) / nullif(count(candidate), 0), 4) AS distinct_ratio
FROM "candidate_table";

SELECT candidate, count(*) AS occurrences
FROM "candidate_table"
WHERE candidate IS NOT NULL AND trim(CAST(candidate AS TEXT)) <> ''
GROUP BY candidate
ORDER BY occurrences DESC
LIMIT 20;
```

Prefer recurring human-readable names, addresses, handles, or organizations. Reject booleans, delivery states, MIME types, and opaque IDs as final entities. An opaque key may join to an entity.

Treat junk sentinels as evidence, not entities. During exploration, decide which values mean “missing” for this database, then freeze that exact cleanup in the contract SQL. For example:

```sql
NULLIF(
  CASE
    WHEN lower(trim(CAST(p.display_name AS TEXT))) IN ('[]', 'unknown') THEN ''
    ELSE trim(CAST(p.display_name AS TEXT))
  END,
  ''
) AS sender
```

Do not add sentinels speculatively. Show their frequencies and get confirmation with the rest of the binding.

### Find timestamp candidates

Measure storage and range before converting:

```sql
SELECT typeof(candidate) AS storage_type, count(*) AS rows
FROM "candidate_table"
GROUP BY storage_type;

SELECT min(CAST(candidate AS INTEGER)) AS min_value,
       max(CAST(candidate AS INTEGER)) AS max_value,
       length(CAST(max(CAST(candidate AS INTEGER)) AS TEXT)) AS max_digits
FROM "candidate_table"
WHERE candidate IS NOT NULL;
```

Use magnitude only to form hypotheses, then prove them on minimum, recent, maximum, and joined samples:

| Likely shape | SQL to Unix milliseconds |
|---|---|
| Unix seconds (~10 digits) | `CAST(value * 1000 AS INTEGER)` |
| Unix milliseconds (~13) | `CAST(value AS INTEGER)` |
| Unix microseconds (~16) | `CAST(value / 1000 AS INTEGER)` |
| Unix nanoseconds (~19) | `CAST(value / 1000000 AS INTEGER)` |
| Apple 2001 seconds | `CAST(value * 1000 AS INTEGER) + 978307200000` |
| Apple 2001 nanoseconds | `CAST(value / 1000000 AS INTEGER) + 978307200000` |

Apple epoch is `2001-01-01T00:00:00Z`; its Unix offset is `978307200000` ms. Never infer it from an Apple-looking schema. File mtime can reflect a copy, restore, or WAL checkpoint and is weak evidence. Prefer comparing converted dates with content-internal references and the source database's own `max(timestamp)`; use source-file recency only as a secondary cross-check. Handle mixed units, negative sentinels, local-time strings, timezone offsets, and Cocoa floating-point seconds explicitly.

```sql
SELECT candidate AS raw,
       <millisecond_expression> AS unix_ms,
       datetime(<millisecond_expression> / 1000, 'unixepoch') AS utc
FROM "candidate_table"
WHERE candidate IS NOT NULL
ORDER BY candidate
LIMIT 10;
```

Use these conversions for evidence display only. The frozen query outputs the timestamp column RAW; the contract's `timestamp` table declares its unit (`s`, `ms`, `us`, `ns`) and epoch, and native execution performs the conversion. Do not bake conversion arithmetic into the contract SQL. Native execution rejects null or invalid timestamps rather than silently substituting indexing time.

### Find content candidates

```sql
SELECT count(*) AS rows,
       sum(CASE WHEN candidate IS NOT NULL AND trim(CAST(candidate AS TEXT)) <> '' THEN 1 ELSE 0 END) AS nonempty,
       min(length(CAST(candidate AS TEXT))) AS min_len,
       round(avg(length(CAST(candidate AS TEXT))), 1) AS avg_len,
       max(length(CAST(candidate AS TEXT))) AS max_len
FROM "candidate_table";

SELECT length(CAST(candidate AS TEXT)) AS chars,
       substr(replace(replace(CAST(candidate AS TEXT), char(10), ' '), char(13), ' '), 1, 160) AS sample
FROM "candidate_table"
WHERE candidate IS NOT NULL AND trim(CAST(candidate AS TEXT)) <> ''
ORDER BY chars DESC
LIMIT 12;
```

Reject blobs, encoded payloads, and status fields. Bind multiple content columns only when their order is stable and intelligible.

### Discover normalized joins

Start with declared foreign keys, then test likely pairs:

```sql
SELECT count(*) AS base_rows,
       count(p.rowid) AS resolved_rows,
       count(DISTINCT m.person_id) AS distinct_keys,
       count(DISTINCT p.display_name) AS distinct_names
FROM messages AS m
LEFT JOIN people AS p ON p.rowid = m.person_id;

WITH joined AS (
  SELECT m.rowid AS message_rowid,
         p.display_name AS sender,
         m.sent_at AS sent_at,
         datetime(<millisecond_expression> / 1000, 'unixepoch') AS utc,
         substr(m.body, 1, 160) AS body,
         t.name AS thread
  FROM messages AS m
  LEFT JOIN people AS p ON p.rowid = m.person_id
  LEFT JOIN threads AS t ON t.rowid = m.thread_id
),
where_ranked AS (
  SELECT joined.*,
         dense_rank() OVER (ORDER BY thread) AS where_rank,
         row_number() OVER (PARTITION BY thread ORDER BY sent_at, message_rowid) AS row_in_where
  FROM joined
  WHERE thread IS NOT NULL AND trim(CAST(thread AS TEXT)) <> ''
)
SELECT 'oldest' AS sample_kind, *
FROM (SELECT * FROM joined ORDER BY sent_at, message_rowid LIMIT 4)
UNION ALL
SELECT 'newest', *
FROM (SELECT * FROM joined ORDER BY sent_at DESC, message_rowid DESC LIMIT 4)
UNION ALL
SELECT 'null-who', *
FROM (SELECT * FROM joined WHERE sender IS NULL ORDER BY sent_at, message_rowid LIMIT 2)
UNION ALL
SELECT 'distinct-where', message_rowid, sender, sent_at, utc, body, thread
FROM where_ranked
WHERE where_rank <= 2 AND row_in_where = 1;
```

Replace the illustrative columns with the proposed join. This union deliberately permits overlap: it guarantees auditable oldest and newest slices, NULL-`who` rows when they exist, and one row from each of at least two distinct `where` values when the data has them. If a required category has no rows, report that fact rather than implying it was sampled.

A good join preserves base cardinality and resolves IDs to names. Investigate fan-out. Aggregate many-to-many values with SQLite JSON functions or a correlated subquery. Final SQL must return exactly one source record per row with unique aliases.

Prove identity separately:

```sql
SELECT count(*) AS rows,
       count(message_rowid) AS nonnull_ids,
       count(DISTINCT message_rowid) AS distinct_ids
FROM (<proposed single-row-per-record query>);
```

Require all three counts to match. Prefer a declared `INTEGER PRIMARY KEY`. Bare `rowid` can change after `VACUUM` unless it aliases one.

## 2. Bind and show evidence

Before mutation, present:

1. Source path, base table/view, row count, and join coverage.
2. Every output alias, universal role, source expression, null rate, relevant distinct count, and rationale.
3. The union sample above: at least 8 joined rows spanning oldest/newest, plus NULL-`who` rows and at least two distinct `where` values when present. Redact only the presentation; do not silently change SQL.
4. Raw timestamp range, inferred unit/epoch, exact millisecond expression, and readable UTC samples.
5. Stable identity result column(s) and uniqueness evidence.
6. Workspace shape/path and whether `where` becomes a collection.

Ask the user to confirm or correct. Correction returns to exploration. A yes authorizes only the config contract, optional standalone directory, and subsequent Enzyme state. It never authorizes source mutation.

## 3. Freeze the native contract

After confirmation, create a missing standalone home with `mkdir -p "$WORKSPACE_HOME"`. Resolve the config location once:

```bash
ENZYME_CONFIG_HOME="${ENZYME_HOME:-$HOME/.enzyme}"
CONFIG_PATH="$ENZYME_CONFIG_HOME/config.toml"
mkdir -p "$ENZYME_CONFIG_HOME"
```

Merge the contract under the canonical absolute workspace key without disturbing existing settings. If the vault section is new, append the reviewed stanza with a quoted heredoc so shell expansion cannot alter its SQL or TOML:

```bash
cat <<'TOML' >> "$CONFIG_PATH"
# Paste the exact confirmed stanza here.
TOML
```

If the vault section or source name already exists, do not append a duplicate TOML table. Back up first with `cp -- "$CONFIG_PATH" "$CONFIG_PATH.bak"`, edit the existing section in place, and keep unrelated settings intact. Then re-read the edited range and run a non-writing config parse check:

```bash
sed -n '/\[vaults\."\/absolute\/path\/to\/workspace-home"\]/,/^\[/p' "$CONFIG_PATH"
"$ENZYME_BIN" -p "$WORKSPACE_HOME" scan
```

`scan` without `--write-config` is the dry run. Treat any TOML/config parse error as a failed merge and restore or correct from the backup before continuing. Its ordinary scan suggestions are not authorization to edit the confirmed contract.

The stanza shape is:

```toml
[vaults."/absolute/path/to/workspace-home".sources.messages]
db = "/absolute/path/to/source.sqlite"
# m.sent_ns is Apple-epoch nanoseconds; declared below, converted by the engine.
query = """
SELECT
  m.rowid AS message_rowid,
  NULLIF(
    CASE WHEN lower(trim(CAST(p.name AS TEXT))) IN ('[]', 'unknown') THEN ''
         ELSE trim(CAST(p.name AS TEXT)) END,
    ''
  ) AS sender,
  m.sent_ns AS sent_ns,
  m.body AS body,
  NULLIF(
    CASE WHEN lower(trim(CAST(t.name AS TEXT))) IN ('[]', 'unknown') THEN ''
         ELSE trim(CAST(t.name AS TEXT)) END,
    ''
  ) AS thread
FROM messages AS m
LEFT JOIN people AS p ON p.rowid = m.person_id
LEFT JOIN threads AS t ON t.rowid = m.thread_id
ORDER BY m.rowid
"""

[vaults."/absolute/path/to/workspace-home".sources.messages.roles]
id = ["message_rowid"]
who = ["sender"]
when = "sent_ns"
what = ["body"]
where = ["thread"]

[vaults."/absolute/path/to/workspace-home".sources.messages.timestamp]
unit = "ns"
epoch = "2001-01-01"
```

This is the exact `enzyme.sqlite-source.v1` shape:

- Source root keys: `db` (absolute SQLite path), `query` (one read-only SQL statement), `roles`, and `timestamp`.
- `roles.id`: one or more result columns forming a stable unique tuple (composite keys allowed).
- `roles.who`: optional array of result columns mapped to link entities.
- `roles.when`: exactly one result column, emitted raw (integer or real).
- `roles.what`: non-empty array of result columns concatenated in order.
- `roles.where`: optional array of result columns mapped to collection/folder entities. Non-null values become catalyst-bearing entities, just like Markdown folders.
- Every array role also accepts a bare string as one-column shorthand; serialization emits arrays.
- `timestamp.unit`: unit of the raw `when` value — one of `s`, `ms`, `us`, `ns`.
- `timestamp.epoch`: optional `YYYY-MM-DD` origin for offset epochs (Apple = `2001-01-01`); absent means Unix epoch.
- Human-readable conversion notes belong in TOML comments, not fields — unknown fields are dropped by programmatic config rewrites.

`roles.id` exists so native execution can derive a stable deterministic document ID from source primary keys/rowids and refresh upserts rather than duplicates. It is declarative identity, not a sync cursor. Enzyme owns incremental cursor/state in `.enzyme/enzyme.db`; never add mutable cursor values to TOML.

Additional invariants:

- Query is one `SELECT` or `WITH ... SELECT` and returns one row per document.
- Every configured column is uniquely aliased and present. On every returned row, every `id` component, `when`, and every configured `what` column must be non-null. A `who` or `where` value may be NULL on an individual row; the row still indexes, simply without that entity or collection.
- `roles.id` tuple is stable and unique. The engine namespaces it by workspace source name before deriving document identity.
- The engine performs timestamp conversion from the declared `unit`/`epoch`; the query emits the raw value.
- No application-specific presets or hidden role inference remain.

Show the exact stanza and config path after writing. Re-read it mechanically. Do not run `scan --write-config` afterward unless native config serialization is known to preserve `sources`.

## 4. Initialize and verify

Native source execution is the only ingestion path:

```bash
"$ENZYME_BIN" -p "$WORKSPACE_HOME" init --quiet
"$ENZYME_BIN" -p "$WORKSPACE_HOME" status
"$ENZYME_BIN" -p "$WORKSPACE_HOME" petri
"$ENZYME_BIN" -p "$WORKSPACE_HOME" petri --query "<one real person or organization>"
"$ENZYME_BIN" -p "$WORKSPACE_HOME" catalyze "What changed with <one real person or organization>?"
```

For an existing Markdown workspace, init/refresh must preserve and update both filesystem and SQLite sources. For standalone, zero Markdown files is valid; the source contract supplies the corpus.

Verify without printing private bodies:

```bash
sqlite3 -readonly "$WORKSPACE_HOME/.enzyme/enzyme.db" <<'SQL'
.headers on
.mode column
SELECT count(*) AS documents FROM docs;
SELECT e.name, count(eo.id) AS occurrences
FROM entities e JOIN entity_occurrences eo ON eo.entity_id = e.id
WHERE e.entity_type = 'link'
GROUP BY e.id, e.name
ORDER BY occurrences DESC
LIMIT 20;
SELECT e.name, count(c.id) AS catalysts
FROM entities e LEFT JOIN catalysts c ON c.entity = e.name
WHERE e.entity_type = 'link'
GROUP BY e.id, e.name
ORDER BY catalysts DESC, e.name
LIMIT 20;
SQL
```

Compare document counts with the contract query, confirm expected people have occurrences, and confirm `petri` exposes catalysts. Catalyst work may be backgrounded; use `status` and verify after completion. If native source parsing/execution is unavailable, fails on an empty workspace, or drops either source shape, stop and report the Rust blocker. Do not fall back to `enzyme ingest` or a skill-managed extractor.

## 5. Re-sync

Run:

```bash
"$ENZYME_BIN" -p "$WORKSPACE_HOME" refresh --quiet
"$ENZYME_BIN" -p "$WORKSPACE_HOME" status
```

Enzyme owns source cursors, replay, upsert state, embeddings, and catalyst scheduling in `.enzyme/enzyme.db`. The skill owns none of that mutable state. Re-open exploration only when refresh reports schema drift, a configured result alias disappears, join cardinality changes, or converted dates become implausible. Show new evidence and obtain confirmation before editing the frozen contract.
