---
name: enzyme-sqlite-source
description: Connect an unfamiliar SQLite database to Enzyme without changing the source data. Inspect the database, decide which query columns represent identity, people, time, content, and folders, show real examples for approval, save the mapping in TOML, and verify init and refresh. Use for email, message, CRM, archive, or other structured SQLite sources with no app-specific preset.
---

# Enzyme SQLite Source

The main rule:

> Inspect the source without changing it. Before writing config, show the user real joined rows and explain the timestamps. After the user confirms the mapping, record it in config so `enzyme init` and `enzyme refresh` can repeat the same work without an LLM.

Follow this order:

1. Inspect the database read-only.
2. Identify the person, date, message, folder, and ID columns.
3. Show real examples and ask the user to confirm the mapping.
4. Write the confirmed config.
5. Run `init` once, then use `refresh`.
6. Verify document and embedding counts.
7. Test whether searches for a person are actually about that person.

Enzyme needs these parts from each source row:

- `id`: a stable value that uniquely identifies the row.
- `who`: people, organizations, or other named entities.
- `when`: when the record happened.
- `what`: the text to index.
- `where`: an optional thread, mailbox, folder, or similar group.

Never edit, migrate, vacuum, attach to, or create anything in the source database. Open it read-only. Do not write config, create workspace files, or initialize Enzyme until the user has seen and confirmed the proposed mapping.

## Choose the source and workspace

The path in `[vaults."path"]` is the **workspace home**. Enzyme stores its local `.enzyme` database, embeddings, catalysts, and source-sync state there. The directory does not need to contain Markdown files.

Every reference to the global `~/.enzyme` home in this skill resolves to `$ENZYME_HOME` when that environment variable is set. In particular, the global config is `$ENZYME_HOME/config.toml`; otherwise it is `~/.enzyme/config.toml`. This does not change the workspace-local `$WORKSPACE_HOME/.enzyme` state directory.

There are two supported setups:

1. **Existing Markdown workspace:** add `sources.<name>` beside the workspace's existing entity, log, and exclusion settings. Enzyme must keep both the Markdown documents and the database records.
2. **Standalone SQLite workspace:** after confirmation, create or use an empty directory, add a vault section containing only `sources.<name>`, and initialize it with no Markdown files.

Set absolute paths and check that the database and Enzyme command are available:

```bash
SOURCE_DB="/absolute/path/to/source.sqlite"
WORKSPACE_HOME="/absolute/path/to/workspace-home"
ENZYME_BIN=enzyme
test -f "$SOURCE_DB"
sqlite3 -readonly "$SOURCE_DB" 'PRAGMA query_only; PRAGMA quick_check;'
"$ENZYME_BIN" --version
```

Before the user confirms the mapping, inspect an existing workspace without changing it:

```bash
test ! -e "$WORKSPACE_HOME" || test -d "$WORKSPACE_HOME"
test ! -d "$WORKSPACE_HOME" || find "$WORKSPACE_HOME" -type f -name '*.md' -not -path '*/.enzyme/*' -print
```

For a standalone workspace, create a missing directory only after the user confirms the mapping. If the application is actively using the database, work from a safe snapshot or use SQLite's backup API. Do not copy only the main database file because its WAL file may contain committed data.

## 1. Inspect the database without changing it

Do not trust a column only because it is named `date`, `from_id`, or `text`. Check its values and joins.

### List the schema and count the rows

This first query lists every table, view, and trigger, shows their definitions, and inspects keys and columns on a candidate table. The final command counts that table's rows.

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

Read views and triggers as well as tables. A view may contain the application's intended join. A trigger may explain how timestamps or soft deletion work. Do not assume foreign keys are declared; many application databases omit them.

## 2. Identify the person, date, message, and folder columns

The remaining inspection decides how source rows map to Enzyme. Find `who` (people or organizations), `when` (date), `what` (message text), and optional `where` (folder or thread). Also find a stable `id` and prove that the joins still return one row per record.

### Find possible `who` columns

For each promising text column or foreign key, check how often it has a value, how many different values it has, and which values appear most often:

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

Good `who` values are repeated human-readable names, addresses, handles, or organizations. Do not use booleans, delivery states, MIME types, or opaque IDs as entities. An opaque ID is useful only when it joins to a readable name.

Some databases use values such as an empty string, `[]`, or `unknown` instead of NULL. Count and show these values during inspection. If the user agrees that they mean “missing,” turn only those confirmed values into NULL in the saved query. For example:

```sql
NULLIF(
  CASE
    WHEN lower(trim(CAST(p.display_name AS TEXT))) IN ('[]', 'unknown') THEN ''
    ELSE trim(CAST(p.display_name AS TEXT))
  END,
  ''
) AS sender
```

Do not guess which values are junk. Show how often they occur and include the decision in the mapping the user approves.

### Find the `when` column

First check how the candidate timestamp is stored and its minimum and maximum values:

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

The number of digits can suggest a unit, but it is not proof. Test the oldest, newest, and recent values, and compare them with joined record content:

| Likely shape | SQL to Unix milliseconds |
|---|---|
| Unix seconds (~10 digits) | `CAST(value * 1000 AS INTEGER)` |
| Unix milliseconds (~13) | `CAST(value AS INTEGER)` |
| Unix microseconds (~16) | `CAST(value / 1000 AS INTEGER)` |
| Unix nanoseconds (~19) | `CAST(value / 1000000 AS INTEGER)` |
| Apple 2001 seconds | `CAST(value * 1000 AS INTEGER) + 978307200000` |
| Apple 2001 nanoseconds | `CAST(value / 1000000 AS INTEGER) + 978307200000` |

Apple's epoch starts at `2001-01-01T00:00:00Z`, which is `978307200000` milliseconds after the Unix epoch. Do not assume Apple time just because the schema looks Apple-related. A database file's modification time may reflect a copy, restore, or WAL checkpoint, so it is weak evidence. Prefer dates mentioned inside the records and the database's own maximum timestamp. Use file recency only as a secondary check. Investigate mixed units, negative sentinel values, local-time strings, timezone offsets, and Cocoa floating-point seconds instead of silently accepting them.

Use this query to show the raw value, the proposed Unix-millisecond value, and the readable UTC date side by side:

```sql
SELECT candidate AS raw,
       <millisecond_expression> AS unix_ms,
       datetime(<millisecond_expression> / 1000, 'unixepoch') AS utc
FROM "candidate_table"
WHERE candidate IS NOT NULL
ORDER BY candidate
LIMIT 10;
```

Use these conversions only while showing evidence. The saved query must return the original timestamp value. Put its unit (`s`, `ms`, `us`, or `ns`) and optional epoch in the `timestamp` config table, and let Enzyme convert it. Do not put timestamp conversion arithmetic in the saved SQL. Enzyme rejects a row whose timestamp is NULL or invalid; it does not silently replace it with indexing time.

### Find the `what` column

These queries show how often the candidate contains usable text, its typical length, and a few of its longest examples:

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

Do not index blobs, encoded payloads, or status fields as content. Use more than one content column only when joining them in a fixed order produces text a person can understand.

### Work out the joins

Start with declared foreign keys, then test likely relationships yourself. The first query below checks whether person IDs resolve to names. The second builds the exact kinds of rows you will show the user.

```sql
SELECT count(*) AS base_rows,
       count(p.rowid) AS resolved_rows,
       count(DISTINCT m.person_id) AS distinct_keys,
       count(DISTINCT p.display_name) AS distinct_names
FROM messages AS m
LEFT JOIN people AS p ON p.rowid = m.person_id;

WITH joined AS (
  SELECT m.rowid AS message_rowid,
         NULLIF(
           CASE WHEN lower(trim(CAST(p.display_name AS TEXT))) IN ('[]', 'unknown') THEN ''
                ELSE trim(CAST(p.display_name AS TEXT)) END,
           ''
         ) AS sender,
         m.sent_at AS sent_at,
         datetime(<millisecond_expression> / 1000, 'unixepoch') AS utc,
         substr(m.body, 1, 160) AS body,
         NULLIF(
           CASE WHEN lower(trim(CAST(t.name AS TEXT))) IN ('[]', 'unknown') THEN ''
                ELSE trim(CAST(t.name AS TEXT)) END,
           ''
         ) AS thread
  FROM messages AS m
  LEFT JOIN people AS p ON p.rowid = m.person_id
  LEFT JOIN threads AS t ON t.rowid = m.thread_id
  WHERE m.sent_at IS NOT NULL AND m.body IS NOT NULL
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

Replace the example columns with the columns you are considering. The same row may appear in more than one part of this sample, which is fine. The query always asks for the oldest rows, newest rows, rows with no `who`, and examples from at least two `where` values when those rows exist. If the database has no rows in one of those groups, say so.

A correct join keeps the same number of base records and turns IDs into readable names. If one base record becomes several result rows, find out why. Combine many-to-many values with SQLite JSON functions or a correlated subquery. The final query must return exactly one row for each source record, and every selected column must have a unique name.

Next, prove that the proposed `id` is present and unique. This query should return the same number in all three columns:

```sql
SELECT count(*) AS rows,
       count(message_rowid) AS nonnull_ids,
       count(DISTINCT message_rowid) AS distinct_ids
FROM (<proposed single-row-per-record query>);
```

All three counts must match. Prefer a declared `INTEGER PRIMARY KEY`. A bare `rowid` may change after `VACUUM` unless it is an alias for that primary key.

## 3. Show real examples and ask the user to confirm

Keep two layers: a complete evidence record for the work, and a short confirmation message for the user. Do not make the user read the evidence record to understand the choice.

### Keep the technical evidence

Before writing anything, gather and retain:

1. Source path, base table/view, row count, and join coverage.
2. Every output column, its Enzyme role, its source expression, its NULL rate, its useful distinct-value count, and why it fits.
3. The union sample above: at least 8 joined rows spanning oldest/newest, plus NULL-`who` rows and at least two distinct `where` values when present. Redact only the presentation; do not silently change SQL.
4. Raw timestamp range, inferred unit/epoch, exact millisecond expression, and readable UTC samples.
5. Stable identity result column(s) and uniqueness evidence.
6. Workspace shape/path and whether `where` becomes a collection.

This evidence is the technical appendix. Keep it in the working record and offer it if the user asks. Do not lead the confirmation message with column names, SQL expressions, NULL rates, units, epochs, or Enzyme role names.

### Reveal the archive in the user's terms

Open with what the inspection found in language the user already uses: how much material there is, the years it covers, who is named, what text Enzyme will read, and which familiar mailboxes, threads, folders, or collections the entries belong to. Use real names and places from the source. Translate missing values into ordinary statements such as “1,403 entries do not name a person.”

Then explain what this makes possible and name only the choices that need judgment. For example:

> Your 3,000 entries run from 2017 to 2026 across Books, inbox, work, and 28 other places. Enzyme will read the entry text, follow the people named in it, and keep each entry with its familiar place, so you can ask how a relationship or thread changed over time. Twelve entries use `[]` where a person would normally be named; I would treat those as “no person.” Does that match how you understand this archive?

Keep this reveal short enough to read aloud in under a minute. Add two or three representative examples only when they make the interpretation easier to judge. Do not call the mapping “healthy” or “valid” in place of showing what was found.

The confirmation still gates every write. If the user corrects the interpretation, return to inspection, update the technical evidence, and present a new reveal. If they approve it, you may write the config, create the standalone workspace directory if needed, and let Enzyme create its own local state. Approval never allows changes to the source database.

## 4. Write the confirmed config

After confirmation, create a missing standalone workspace with `mkdir -p "$WORKSPACE_HOME"`. Then find the correct global config path:

```bash
ENZYME_CONFIG_HOME="${ENZYME_HOME:-$HOME/.enzyme}"
CONFIG_PATH="$ENZYME_CONFIG_HOME/config.toml"
mkdir -p "$ENZYME_CONFIG_HOME"
```

Save the confirmed mapping under the workspace's absolute path without changing unrelated settings. If this workspace has no vault section yet, append the reviewed text with a quoted heredoc. The quotes stop the shell from changing characters inside the SQL or TOML:

```bash
cat <<'TOML' >> "$CONFIG_PATH"
# Paste the exact confirmed stanza here.
TOML
```

If the vault section or source name already exists, do not add a second copy. First run `cp -- "$CONFIG_PATH" "$CONFIG_PATH.bak"`. Then edit the existing section and leave unrelated settings alone. After either kind of edit, print the result and ask Enzyme to read it without writing anything:

```bash
sed -n '/\[vaults\."\/absolute\/path\/to\/workspace-home"\]/,$p' "$CONFIG_PATH"
"$ENZYME_BIN" -p "$WORKSPACE_HOME" scan
```

Here, `scan` is a dry run because it does not include `--write-config`. If Enzyme reports a TOML or config error, stop and either fix it or restore the backup. Scan suggestions do not give permission to change the mapping the user approved.

Write the config in exactly this shape:

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
WHERE m.sent_ns IS NOT NULL AND m.body IS NOT NULL
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

The exact `enzyme.sqlite-source.v1` rules are:

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

Enzyme uses `roles.id` to make the same document ID every time it sees a source record. That lets `refresh` update a document instead of creating a duplicate. `roles.id` describes identity; it is not the changing sync cursor. Enzyme stores that cursor in `.enzyme/enzyme.db`. Never copy cursor values into TOML.

The query must also follow these rules:

- Query is one `SELECT` or `WITH ... SELECT` and returns one row per document.
- Every configured column is uniquely aliased and present. On every returned row, every `id` component, `when`, and every configured `what` column must be non-null. A `who` or `where` value may be NULL on an individual row; the row still indexes, simply without that entity or collection.
- The `roles.id` tuple is stable and unique. Enzyme combines it with the workspace source name before creating the document ID.
- Enzyme converts time using the declared `unit` and `epoch`; the query returns the raw value.
- The config contains every application-specific decision. Nothing depends on a hidden preset or later guessing.

After writing, show the exact config path and the text you added. Read it back from disk. Do not run `scan --write-config` afterward unless you know that the installed Enzyme version preserves `sources` when it rewrites config.

## 5. Initialize once, then use refresh

Use this configured source as the only ingestion path. Run `init` once for a workspace. Use `refresh` after that. Running `init` again regenerates and **replaces** catalysts, so their wording and counts may change even when the config and source data did not. `refresh` is the normal, repeatable way to sync again.

```bash
"$ENZYME_BIN" -p "$WORKSPACE_HOME" init --quiet
"$ENZYME_BIN" -p "$WORKSPACE_HOME" status
"$ENZYME_BIN" -p "$WORKSPACE_HOME" petri
```

In an existing Markdown workspace, `init` and `refresh` must keep and update both the Markdown files and SQLite records. In a standalone workspace, zero Markdown files is valid because the configured SQLite source supplies the documents.

After the first `init`, use these commands whenever the source changes:

```bash
"$ENZYME_BIN" -p "$WORKSPACE_HOME" refresh --quiet
"$ENZYME_BIN" -p "$WORKSPACE_HOME" status
sqlite3 "$WORKSPACE_HOME/.enzyme/enzyme.db" "SELECT source_name, cursor_kind, cursor_value FROM source_cursors;"
```

The `source_cursors` query shows where Enzyme's incremental sync has reached. This value belongs in `.enzyme/enzyme.db`, not in the config. Enzyme manages the cursor, reprocessing, document updates, embeddings, and catalyst scheduling. If `refresh` reports a schema change, a configured output column disappears, a join starts returning a different number of rows, or converted dates stop making sense, inspect the source again. Show the user new evidence and get confirmation before changing the saved mapping.

## 6. Verify document and embedding counts

Verify the mechanics privately before describing success to the user. The next queries count documents, embeddings, entities, and catalysts without printing private message bodies. Keep their raw output in the technical record; do not make it the user-facing result.

```bash
sqlite3 -readonly "$WORKSPACE_HOME/.enzyme/enzyme.db" <<'SQL'
.headers on
.mode column
SELECT count(*) AS documents,
       coalesce(sum(CASE
         WHEN embedded_at IS NOT NULL AND embedded_content_hash = content_hash THEN 1
         ELSE 0
       END), 0) AS embedded,
       coalesce(sum(CASE
         WHEN embedded_at IS NULL OR embedded_content_hash IS NULL OR embedded_content_hash <> content_hash THEN 1
         ELSE 0
       END), 0) AS awaiting_embedding
FROM docs;
SELECT count(*) AS source_documents
FROM docs
WHERE source_ref LIKE 'sqlite:messages/%';
SELECT e.name, count(eo.id) AS occurrences
FROM entities e JOIN entity_occurrences eo ON eo.entity_id = e.id
WHERE e.type = 'link'
GROUP BY e.id, e.name
ORDER BY occurrences DESC
LIMIT 20;
SELECT e.name, count(c.id) AS catalysts
FROM entities e LEFT JOIN catalysts c ON c.entity = e.name
WHERE e.type IN ('link', 'folder')
GROUP BY e.id, e.name
ORDER BY catalysts DESC, e.name
LIMIT 20;
SQL
```

Replace `messages` with the configured source name. `source_documents` must match the number of rows returned by the saved source query. In a standalone workspace, it should also match total `documents`. In a mixed Markdown workspace, total `documents` includes both sources, so do not compare that combined number directly with the SQLite row count.

The default is `max_embedding_files = 1024`, so one run embeds at most the newest 1024 documents. With `--quiet`, a large source can therefore look finished while older documents still have no embeddings. Require `embedded = documents` and `awaiting_embedding = 0`. If they do not match, run `refresh` and check again until they do. Alternatively, with the user's approval, back up and edit the config as described above and raise `max_embedding_files` for this vault. Set it to `0` to remove the limit:

```toml
[vaults."/absolute/path/to/workspace-home"]
max_embedding_files = 0
```

Do not run `init` again to finish an embedding backlog.

## 7. Test whether person searches are relevant

Keep the exact-name and timeline check private too. Query Enzyme's database directly because `petri --query` uses semantic search and cannot prove that an exact name exists or that its occurrences are correct. The following query proves both the exact match and the person's yearly and monthly coverage:

```bash
sqlite3 -readonly "$WORKSPACE_HOME/.enzyme/enzyme.db" <<'SQL'
.headers on
.mode column
.parameter init
.parameter set :entity 'Exact Person Name'
SELECT e.id, e.name, e.type, count(eo.id) AS occurrences
FROM entities AS e
LEFT JOIN entity_occurrences AS eo
  ON eo.entity_id = e.id AND eo.file_path LIKE 'sqlite:messages/%'
WHERE e.name = :entity COLLATE NOCASE
GROUP BY e.id, e.name, e.type;

WITH person_occurrences AS (
  SELECT eo.effective_date
  FROM entity_occurrences AS eo
  JOIN entities AS e ON e.id = eo.entity_id
  WHERE e.name = :entity COLLATE NOCASE
    AND eo.file_path LIKE 'sqlite:messages/%'
    AND eo.effective_date IS NOT NULL
)
SELECT 'year' AS grain,
       strftime('%Y', effective_date / 1000, 'unixepoch') AS era,
       count(*) AS occurrences
FROM person_occurrences
GROUP BY era
UNION ALL
SELECT 'month',
       strftime('%Y-%m', effective_date / 1000, 'unixepoch'),
       count(*)
FROM person_occurrences
GROUP BY strftime('%Y-%m', effective_date / 1000, 'unixepoch')
ORDER BY grain, era;
SQL
```

`effective_date` is Unix milliseconds. Replace `messages` with the configured source name. Compare the yearly and monthly counts with what you learned from the source. A top person should appear across the expected periods.

Now test the user-facing search. A query for a top person must return material that is mostly about that person and follows the expected time periods. If much of the result is unrelated, report that as a problem instead of calling the test successful.

```bash
"$ENZYME_BIN" -p "$WORKSPACE_HOME" petri --query "<one exact top person name>"
"$ENZYME_BIN" -p "$WORKSPACE_HOME" catalyze "What changed with <one exact top person name>?"
```

After the private checks, close with what became possible in the user's language. Name the full span, a real person with verified dated entries, a few familiar places, and one question worth trying. For example:

> Your full archive is here. Enzyme can now follow Me across 781 dated entries from 2018 to 2026 and treat Books, inbox, and work as recurring places. Try asking: “What changed in my work with Will Ashby?”

Use that positive reveal only when the example actually works. If it fails, say what happened in the same plain language: “The full archive is here, but that question returned mostly unrelated people, so person-focused search is not working well yet.” The reveal is the headline; document, embedding, occurrence, entity, and catalyst counts stay in the technical appendix unless the user asks for them.

If the exact occurrence timeline contains unrelated records, the `who` mapping or cleanup rule may be mixing people together. If the exact timeline is correct but `petri` or `catalyze` still returns unrelated material, report a retrieval or ranking failure instead of reopening a proven mapping. If Enzyme cannot read the configured source, cannot initialize an empty standalone workspace, or loses either Markdown or SQLite documents, stop and report that the implementation needs a Rust fix. Do not switch to `enzyme ingest` or write a custom extractor in the skill.
