---
name: enzyme-sqlite-source
description: Connect an unfamiliar SQLite database to Enzyme without changing the source data. Inspect the database, decide which query columns represent identity, people, time, content, and folders, show real examples for approval, save the mapping in TOML, and verify init and refresh. Use for email, message, CRM, archive, or other structured SQLite sources with no app-specific preset.
---

# Enzyme SQLite Source

The main rule:

> Inspect the source without changing it. Before writing config, show the user real joined rows and explain the timestamps. After the user confirms the mapping, record it in config so `enzyme init` and `enzyme refresh` can repeat the same work without an LLM.

The first job of every user-facing message is revelation, not a report about the engine. Speak in the vocabulary of the archive: its people, dates, messages, mailboxes, threads, folders, and questions. Column names, SQL, NULL rates, source row/bucket counts, embedding counts, entities, and catalysts are evidence for the agent. Translate what they mean before speaking to the user, and keep the raw details in the technical appendix unless the user asks for them.

Follow this order:

1. Inspect the database read-only.
2. Identify the person, date, message, folder, and ID columns and choose a workspace name.
3. Show real examples and ask the user to confirm the mapping.
4. Write only the confirmed source contract under `[workspaces.<name>.sources.<source>]`.
5. Validate it with `enzyme scan --collection <name>`.
6. Build it with `enzyme --collection <name> init`, then use `refresh`.
7. Verify document and embedding counts.
8. Test whether searches for a person are actually about that person.

Enzyme needs these parts from each source row:

- `id`: a stable value that uniquely identifies the row.
- `who`: people, organizations, or other named entities.
- `when`: when the record happened.
- `what`: the text to index.
- `where`: an optional thread, mailbox, folder, or similar group.

Never edit, migrate, vacuum, attach to, or create anything in the source database. Open it read-only. Do not write config or initialize Enzyme until the user has seen and confirmed the proposed mapping.

## Choose the source and workspace

Choose a short workspace name such as `imessage`, `mail`, or `crm`. It must be one filesystem component, not a path. Address it only as `--collection <name>`. Enzyme owns its index at `$ENZYME_HOME/workspaces/<name>/enzyme.db` (default `~/.enzyme/workspaces/<name>/enzyme.db`); the skill never chooses, creates, or writes that storage path.

The global config is `$ENZYME_HOME/config.toml` when `ENZYME_HOME` is set and `~/.enzyme/config.toml` otherwise. The skill writes one source contract under `[workspaces.<name>.sources.<source>]`. It never writes `entities`, `excluded_*`, `catalyst_format`, `max_embedding_files`, targets, or storage paths. `init` derives or defaults those fields and persists them. For SQLite sources, mapped `who` values deterministically become people and mapped `where` values become threads/folders during indexing; there is no agent task to enumerate them.

Set absolute paths and check that the database and Enzyme command are available:

```bash
SOURCE_DB="/absolute/path/to/source.sqlite"
WORKSPACE_NAME="imessage"
ENZYME_BIN=enzyme
test -f "$SOURCE_DB"
sqlite3 -readonly "$SOURCE_DB" 'PRAGMA query_only; PRAGMA quick_check;'
"$ENZYME_BIN" --version
```

`sqlite3 -readonly` is the safeguard that prevents source writes. The displayed `PRAGMA query_only` value may be `0`; do not present that value as proof that the source was writable or read-only. `PRAGMA quick_check` checks database integrity, not write protection.

If the application is actively using the database, work from a safe snapshot or use SQLite's backup API. Do not copy only the main database file because its WAL file may contain committed data.

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
6. Workspace name and whether `where` becomes a thread/folder.

This evidence is the technical appendix. Keep it in the working record and offer it if the user asks. Do not lead the confirmation message with column names, SQL expressions, NULL rates, units, epochs, or Enzyme role names.

### Reveal the archive in the user's terms

Assume the user has never heard of Enzyme, timelines, entities, eras, embeddings, or catalysts. Open with what the inspection found in language they already use: how much material there is, the years it covers, who is named, what text Enzyme will read, and which familiar mailboxes, threads, folders, or collections the entries belong to. Use real names and places from the source. Translate missing values into ordinary statements such as “1,403 entries do not name a person.”

Explain the significance before asking for approval. A date turns an entry into a moment that can be placed before or after other moments. A repeated person gathers those moments into the history of a relationship. A repeated mailbox, thread, or folder gathers them into the history of a place. Pair each important person or place you name with a concrete question that this structure is intended to answer. Make clear that confirmation sets up these capabilities and that you will test them after initialization before claiming they work.

Then name only the choices that need the user's judgment. For example:

> Your archive contains 3,000 entries from 2017 to 2026, naming 137 people across 31 familiar places such as Books, inbox, and work. If you confirm this reading, Enzyme will treat each entry as a dated moment. That sets it up to follow one person across years — “How did my work with [real person] change after 2021?” — or follow one place over time — “What was I collecting in Books during 2021?” Twelve entries use `[]` where a person would normally be named; I would treat those as “no person.” After setup I will test the example questions before saying they work. Does this match how you understand your archive?

The example above shows the shape, not facts to copy. Build the real version from the source evidence. Keep it short enough to read aloud in under a minute. Add two or three representative entries only when they make the interpretation easier to judge. Do not call the mapping “healthy” or “valid” in place of showing what was found and why it matters.

The confirmation still gates every write. If the user corrects the interpretation, return to inspection, update the technical evidence, and present a new reveal. If they approve it, write the source contract and let Enzyme create its own local state. Approval never allows changes to the source database.

## 4. Write the confirmed config

After confirmation, find the correct global config path:

```bash
ENZYME_CONFIG_HOME="${ENZYME_HOME:-$HOME/.enzyme}"
CONFIG_PATH="$ENZYME_CONFIG_HOME/config.toml"
mkdir -p "$ENZYME_CONFIG_HOME"
```

Save the confirmed mapping under the chosen workspace name without changing unrelated settings. The only durable output from this skill is the source contract: `db`, `query`, `roles`, and `timestamp`. If this source section does not exist, append the reviewed text. If it exists, back up the config and edit that section in place; never add a duplicate and never replace the rest of `[workspaces.<name>]`.

```bash
cat <<'TOML' >> "$CONFIG_PATH"
# Paste the exact confirmed stanza here.
TOML
```

After the edit, re-read the written section and validate it read-only. Record the exact config path and the source contract's full text in the technical appendix; do not show raw TOML to the user unless they ask:

```bash
awk -v key="$WORKSPACE_NAME" '
  /^\[workspaces\./ {
    same_workspace = index($0, "[workspaces." key) == 1
    if (showing && !same_workspace) exit
    if (same_workspace) showing = 1
  }
  showing { print }
' "$CONFIG_PATH"
"$ENZYME_BIN" scan --collection "$WORKSPACE_NAME"
```

`scan --collection` opens the source read-only, runs the query, resolves every configured role column, and reports the row count in `source_checks`. Require every source check to pass before initialization. Never use `scan --write-config` for SQLite: it is a Markdown-structure heuristic, not a SQLite contract author.

Write the config in exactly this shape:

```toml
[workspaces.imessage.sources.messages]
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

[workspaces.imessage.sources.messages.roles]
id = ["message_rowid"]
who = ["sender"]
when = "sent_ns"
what = ["body"]
where = ["thread"]

[workspaces.imessage.sources.messages.timestamp]
unit = "ns"
epoch = "2001-01-01"
```

The exact `enzyme.sqlite-source.v1` rules are:

- Source root keys: `db` (absolute SQLite path), `query` (one read-only SQL statement), `roles`, and `timestamp`.
- `roles.id`: one or more result columns forming a stable unique tuple (composite keys allowed).
- `roles.who`: optional array of result columns mapped to link entities.
- `roles.when`: exactly one result column, emitted raw (integer or real).
- `roles.what`: non-empty array of result columns concatenated in order.
- `roles.where`: optional array of result columns mapped to thread/collection entities. Defining it automatically enables log mode: Enzyme groups rows by those values and UTC calendar month, producing one conversation document per thread-month. Rows whose `where` values are NULL share a per-month fallback bucket. Non-null values also become catalyst-bearing folder entities.
- Every array role also accepts a bare string as one-column shorthand; serialization emits arrays.
- `timestamp.unit`: unit of the raw `when` value — one of `s`, `ms`, `us`, `ns`.
- `timestamp.epoch`: optional `YYYY-MM-DD` origin for offset epochs (Apple = `2001-01-01`); absent means Unix epoch.
- Human-readable conversion notes belong in TOML comments, not fields — unknown fields are dropped by programmatic config rewrites.

Without `roles.where`, Enzyme uses `roles.id` to make the same per-row document ID every time it sees a source record. With `roles.where`, document identity comes from the source name, thread values, and UTC month; `roles.id` is only the stable tie-breaker for messages with the same timestamp. Refresh compares deterministic content fingerprints and does not store a source cursor.

The query must also follow these rules:

- Query is one `SELECT` or `WITH ... SELECT` and returns one row per record/message. A source with `roles.where` groups those rows into thread-month documents.
- Every configured column is uniquely aliased and present. On every returned row, every `id` component and `when` must be non-null. A row whose mapped `what` values are all NULL/empty is skipped and counted. A `who` or `where` value may be NULL on an individual row; the row still indexes, simply without that entity or thread.
- The `roles.id` tuple is stable and unique. It identifies documents for sources without `roles.where` and deterministically orders equal-time entries in log mode.
- Enzyme converts time using the declared `unit` and `epoch`; the query returns the raw value.
- The source contract contains every application-specific schema decision. Derived entity/exclusion/format/default fields are deliberately owned by `init`, not by this skill.

After writing, show the exact config path and the source contract you added. Read it back from disk and require the read-only scan to pass.

## 5. Initialize once, then use refresh

Use this configured source as the only ingestion path. Run `init` once for a workspace. Use `refresh` after that. Running `init` again regenerates and **replaces** catalysts, so their wording and counts may change even when the config and source data did not. `refresh` is the normal, repeatable way to sync again.

```bash
"$ENZYME_BIN" --collection "$WORKSPACE_NAME" init --quiet
"$ENZYME_BIN" --collection "$WORKSPACE_NAME" status
"$ENZYME_BIN" --collection "$WORKSPACE_NAME" petri
```

Zero Markdown files is valid because the configured SQLite source supplies the documents. If another setup flow already configured a `sources.notes` path in this named workspace, `init` and `refresh` must keep and update both Markdown and SQLite documents; this skill does not author that notes contract.

After the first `init`, use these commands whenever the source changes:

```bash
"$ENZYME_BIN" --collection "$WORKSPACE_NAME" refresh --quiet
"$ENZYME_BIN" --collection "$WORKSPACE_NAME" status
```

`status` reports each source's row count, bucket count, and last refresh time. Enzyme manages fingerprint comparison, document rebuilds/pruning, embeddings, and catalyst scheduling. If `refresh` reports a schema change, a configured output column disappears, a join starts returning a different number of rows, or converted dates stop making sense, inspect the source again. Show the user new evidence and get confirmation before changing the saved mapping.

## 6. Verify document and embedding counts

Verify the mechanics privately before describing success to the user. The next queries count documents, embeddings, entities, and catalysts without printing private message bodies. Keep their raw output in the technical record; do not make it the user-facing result.

```bash
WORKSPACE_DB="${ENZYME_HOME:-$HOME/.enzyme}/workspaces/$WORKSPACE_NAME/enzyme.db"
sqlite3 -readonly "$WORKSPACE_DB" <<'SQL'
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
SELECT e.name, e.type, count(eo.id) AS occurrences,
       (SELECT count(*) FROM catalysts c WHERE c.entity = e.name) AS catalysts
FROM entities e JOIN entity_occurrences eo ON eo.entity_id = e.id
WHERE e.type IN ('link', 'folder')
GROUP BY e.id, e.name, e.type
ORDER BY occurrences DESC
LIMIT 20;
SELECT count(*) AS collections,
       coalesce(sum(CASE WHEN EXISTS (
         SELECT 1 FROM catalysts c WHERE c.entity = e.name
       ) THEN 1 ELSE 0 END), 0) AS collections_with_catalysts,
       coalesce(sum(CASE WHEN NOT EXISTS (
         SELECT 1 FROM catalysts c WHERE c.entity = e.name
       ) THEN 1 ELSE 0 END), 0) AS collections_without_catalysts
FROM entities e
WHERE e.type = 'folder';
SQL
```

Replace `messages` with the configured source name. For a source without `roles.where`, `source_documents` must match the number of rows returned by the saved source query. For a source with `roles.where` (log mode), `source_documents` is the bucket count — one document per thread-month — so it will be far smaller than the row count; compare it with the row and bucket counts that `status` reports for the source instead. In a standalone workspace, `source_documents` should also match total `documents`. In a mixed Markdown workspace, total `documents` includes both sources, so do not compare that combined number directly with the source's own counts.

The default is `max_embedding_files = 1024`, so one run embeds at most the newest 1024 documents. With `--quiet`, a large source can therefore look finished while older documents still have no embeddings. Require `embedded = documents` and `awaiting_embedding = 0`. If they do not match, run `refresh` and check again until they do. Do not author or change `max_embedding_files` in this skill, and do not run `init` again to finish an embedding backlog.

Catalysts are generated selectively and may finish in the background, so not every collection is expected to receive one. Use this pass bar:

1. `status` shows that catalyst generation completed successfully.
2. Every high-occurrence person or collection returned by the top-20 query has at least one catalyst. A top entity with zero catalysts after completion is a finding to report.
3. Record `collections_without_catalysts` in the technical appendix. A nonzero count is useful coverage information, not a failure by itself.

## 7. Test whether person searches are relevant

Keep the exact-name and timeline check private too. Query Enzyme's database directly because `petri --query` uses semantic search and cannot prove that an exact name exists or that its occurrences are correct. The following query proves both the exact match and the person's yearly and monthly coverage:

```bash
sqlite3 -readonly "$WORKSPACE_DB" <<'SQL'
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
"$ENZYME_BIN" --collection "$WORKSPACE_NAME" petri --query "<one exact top person name>"
"$ENZYME_BIN" --collection "$WORKSPACE_NAME" catalyze "What changed with <one exact top person name>?"
```

Build the post-init reveal from questions that work, not from generic promises. Choose two or three questions grounded in the actual names, places, and dates you verified. When the source has both people and places, pre-test at least one person question and one familiar-place question; include each in the reveal only if it passes, and disclose the failure plainly if it does not. Run each proposed question first, using the same `petri` and `catalyze` checks. Only offer a question when its results are predominantly about the named person or place and use the expected dates.

If every focused person or place question fails, run a positive control before writing the reveal: pick one broad topic that recurs across the archive and test it the same way. A passing broad question shows the archive is searchable at all and belongs in the reveal, clearly distinguished from the failed focused questions — "broad questions about recurring subjects work; questions about one person do not yet."

For a place question, adapt the same check:

```bash
"$ENZYME_BIN" --collection "$WORKSPACE_NAME" petri --query "<one familiar collection name>"
"$ENZYME_BIN" --collection "$WORKSPACE_NAME" catalyze "What was I collecting in <one familiar collection name> during <one supported year>?"
```

After the private checks, explain what became possible as an anchor paired with a question. Start by explaining the simple mechanism: dates put entries in order; repeated people and places gather those dated moments into histories. Then use the user's real data. For example:

> Your archive is now a timeline of 3,000 moments from 2017 to 2026. Dates keep those moments in order; the people and places you already use gather them into histories you can ask about.
>
> [Verified person] appears across 86 entries from 2019 to this year. Ask: “What changed in my work with [verified person] after 2021?”
>
> Books is now a place with its own history. Ask: “What was I collecting in Books during 2021?”

The example above shows the form. Replace every count, date, name, place, and question with evidence from the actual archive, and omit any line whose question did not pass. If a question fails, say what happened in the same plain language: “The full archive is here, but ‘What changed with Will?’ returned mostly unrelated people, so that person-focused question is not working well yet.” The reveal is the headline; document, embedding, occurrence, entity, and catalyst counts stay in the technical appendix unless the user asks for them.

If the exact occurrence timeline contains unrelated records, the `who` mapping or cleanup rule may be mixing people together. If the exact timeline is correct but `petri` or `catalyze` still returns unrelated material, report a retrieval or ranking failure instead of reopening a proven mapping. If Enzyme cannot read the configured source, cannot initialize an empty standalone workspace, or loses either Markdown or SQLite documents, stop and report that the implementation needs a Rust fix. Do not switch to `enzyme ingest` or write a custom extractor in the skill.
