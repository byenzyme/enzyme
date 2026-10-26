/// The complete database schema for Enzyme's main vault database.
///
/// Key improvement over the Python version: tags and links use proper junction
/// tables (`doc_tags`, `doc_links`) instead of JSON blobs, enabling efficient
/// SQL queries and indexing on individual tag/link values.
pub const SCHEMA_SQL: &str = r#"
-- Documents ------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS docs (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    source_ref      TEXT    NOT NULL UNIQUE,
    title           TEXT,
    content         TEXT,
    content_hash    TEXT    NOT NULL,
    word_count      INTEGER DEFAULT 0,
    char_count      INTEGER DEFAULT 0,
    created_at      INTEGER NOT NULL,          -- milliseconds since epoch
    created_source  TEXT    DEFAULT 'filesystem',
    modified_at     INTEGER NOT NULL,
    modified_source TEXT    DEFAULT 'filesystem',
    indexed_at      INTEGER NOT NULL,
    embedded_at     INTEGER,
    embedded_content_hash TEXT,
    metadata        TEXT    DEFAULT '{}'
);
-- Covers folder summaries (latest modified_at per folder) and folder listings.
-- modified_at is stored after the document text, so reading it from the table
-- walks every long document's text pages; on large vaults that is gigabytes.
CREATE INDEX IF NOT EXISTS idx_docs_source_modified ON docs(source_ref, modified_at);

-- Junction tables (replacing JSON blob queries from Python) -------------------
CREATE TABLE IF NOT EXISTS doc_tags (
    doc_id INTEGER NOT NULL REFERENCES docs(id) ON DELETE CASCADE,
    tag    TEXT    NOT NULL,
    PRIMARY KEY (doc_id, tag)
);
CREATE INDEX IF NOT EXISTS idx_doc_tags_tag ON doc_tags(tag);

CREATE TABLE IF NOT EXISTS doc_links (
    doc_id INTEGER NOT NULL REFERENCES docs(id) ON DELETE CASCADE,
    link   TEXT    NOT NULL,
    PRIMARY KEY (doc_id, link)
);
CREATE INDEX IF NOT EXISTS idx_doc_links_link ON doc_links(link);

-- Chunks ----------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS chunks (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    doc_id      INTEGER NOT NULL REFERENCES docs(id) ON DELETE CASCADE,
    path        TEXT,
    chunk_index INTEGER NOT NULL,
    content     TEXT    NOT NULL,
    word_count  INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL,
    entry_date  INTEGER,                       -- log-entry date (millis) for chunks cut from a dated append-log entry
    UNIQUE(doc_id, chunk_index)
);

-- Entities --------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS entities (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    name      TEXT    NOT NULL,
    type      TEXT    NOT NULL,
    frequency INTEGER DEFAULT 1,
    first_seen INTEGER,
    last_seen  INTEGER,
    trends    TEXT    DEFAULT '{}',
    metadata  TEXT    DEFAULT '{}',
    UNIQUE(name, type)
);
CREATE INDEX IF NOT EXISTS idx_entities_name      ON entities(name);
CREATE INDEX IF NOT EXISTS idx_entities_frequency ON entities(frequency DESC);

-- Entity occurrences ----------------------------------------------------------
CREATE TABLE IF NOT EXISTS entity_occurrences (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    entity_id      INTEGER NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    file_path      TEXT    NOT NULL,
    context        TEXT,
    created_at     INTEGER,
    effective_date INTEGER,
    date_source    TEXT    DEFAULT 'filesystem',
    weight         REAL    NOT NULL DEFAULT 1.0
);
-- idx_entity_occurrences_entity_weight lives in POST_MIGRATION_SQL: an older
-- table gains `weight` only after this batch runs.
DROP INDEX IF EXISTS idx_entity_occurrences_entity;
CREATE INDEX IF NOT EXISTS idx_entity_occurrences_file   ON entity_occurrences(file_path);

-- Catalysts -------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS catalysts (
    id           TEXT PRIMARY KEY,
    text         TEXT NOT NULL,
    entity       TEXT NOT NULL,
    context      TEXT DEFAULT 'default',
    generated_at TEXT DEFAULT (datetime('now')),
    metadata     TEXT DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS idx_catalysts_entity  ON catalysts(entity);
CREATE INDEX IF NOT EXISTS idx_catalysts_context ON catalysts(context);

-- Per-entity catalyst context fingerprints ------------------------------------
--
-- One row per (entity, type) whose catalysts were generated successfully. The
-- hash covers everything that determined the LLM output for that entity — its
-- resolved prompts, the model, and the requested catalyst count — so a re-run
-- can skip entities whose inputs are unchanged instead of paying for an LLM
-- call. Rows are written only after the catalysts they describe are committed.
CREATE TABLE IF NOT EXISTS catalyst_entity_hashes (
    entity          TEXT    NOT NULL,
    entity_type     TEXT    NOT NULL,
    context_hash    TEXT    NOT NULL,
    generated_at_ms INTEGER NOT NULL,
    PRIMARY KEY (entity, entity_type)
);

-- Catalyst similarities (typically in the embeddings database) ----------------
CREATE TABLE IF NOT EXISTS catalyst_similarities (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    catalyst_id     TEXT    NOT NULL,
    catalyst_text   TEXT    NOT NULL,
    file_path       TEXT    NOT NULL,
    chunk_index     INTEGER DEFAULT 0,
    similarity      REAL    NOT NULL,
    chunk_metadata  TEXT    DEFAULT '{}',
    timeline_data   TEXT    DEFAULT '{}',
    created_at      TEXT    DEFAULT (datetime('now')),
    UNIQUE(catalyst_id, file_path, chunk_index)
);

-- Context cache ---------------------------------------------------------------
CREATE TABLE IF NOT EXISTS context_cache (
    query_hash   TEXT PRIMARY KEY,
    query        TEXT NOT NULL,
    results      TEXT NOT NULL,
    context_name TEXT DEFAULT 'default',
    generated_at TEXT DEFAULT (datetime('now')),
    expires_at   TEXT
);

-- Vault configuration key-value store -----------------------------------------
CREATE TABLE IF NOT EXISTS config (
    key        TEXT PRIMARY KEY,
    value      TEXT,
    updated_at TEXT DEFAULT (datetime('now'))
);

-- Read-only orientation for configured SQLite sources -------------------------
CREATE TABLE IF NOT EXISTS source_refreshes (
    source_name     TEXT PRIMARY KEY,
    row_count       INTEGER NOT NULL,
    bucket_count    INTEGER NOT NULL,
    refreshed_at_ms INTEGER NOT NULL
);

-- Versioned decisions for decoded SQLite source rows. Text stays in the source
-- and retained documents; this cache stores only its digest and the decision.
CREATE TABLE IF NOT EXISTS source_filter_cache (
    source_name   TEXT NOT NULL,
    policy        TEXT NOT NULL,
    row_key       TEXT NOT NULL,
    text_hash     TEXT NOT NULL,
    is_excluded   INTEGER NOT NULL,
    PRIMARY KEY (source_name, policy, row_key)
);

-- Applied targets (external directories this vault's catalysts have been projected onto)
CREATE TABLE IF NOT EXISTS applied_targets (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    target_path     TEXT NOT NULL UNIQUE,
    doc_count       INTEGER NOT NULL DEFAULT 0,
    catalyst_count  INTEGER NOT NULL DEFAULT 0,
    applied_at      TEXT DEFAULT (datetime('now'))
);

-- Chunk embeddings (replaces embeddings.bin) ------------------------------------
CREATE TABLE IF NOT EXISTS chunk_embeddings (
    file_path   TEXT    NOT NULL,
    chunk_index INTEGER NOT NULL,
    vector      BLOB    NOT NULL,
    PRIMARY KEY (file_path, chunk_index)
);

-- Packed catalyst embeddings: all vectors in a single row to eliminate
-- per-row async overhead on the libsql read path.  `count` vectors (the
-- embedding dimension is determined by the active embedding backend, not
-- fixed by this schema), quantized and stored as described by
-- `crate::embedding::quantize`, alongside a JSON array of catalyst_ids.
CREATE TABLE IF NOT EXISTS catalyst_embeddings_packed (
    id      INTEGER PRIMARY KEY DEFAULT 1,
    count   INTEGER NOT NULL,
    ids     TEXT    NOT NULL,
    vectors BLOB    NOT NULL
);

-- Catalyst epochs ------------------------------------------------------------
--
-- `catalysts`, `catalyst_embeddings_packed`, and `catalyst_similarities` are
-- the atomically-switched active materialized view.  Complete shadow epochs
-- are built in frozen database snapshots; these tables durably describe the
-- active generation and coordinate/coalesce detached builders.
CREATE TABLE IF NOT EXISTS catalyst_epochs (
    epoch_id                    TEXT PRIMARY KEY,
    status                      TEXT NOT NULL,
    corpus_generation           INTEGER NOT NULL,
    corpus_fingerprint          TEXT NOT NULL,
    generation_contract         TEXT NOT NULL,
    generation_contract_json    TEXT NOT NULL DEFAULT '{}',
    snapshot_path               TEXT,
    requested_at_ms             INTEGER NOT NULL,
    started_at_ms               INTEGER,
    completed_at_ms             INTEGER,
    activated_at_ms             INTEGER,
    failed_at_ms                INTEGER,
    attempt                     INTEGER NOT NULL DEFAULT 0,
    next_retry_at_ms            INTEGER,
    last_error                  TEXT
);
CREATE INDEX IF NOT EXISTS idx_catalyst_epochs_status
    ON catalyst_epochs(status);

CREATE TABLE IF NOT EXISTS catalyst_epoch_state (
    singleton                   INTEGER PRIMARY KEY CHECK (singleton = 1),
    corpus_generation           INTEGER NOT NULL DEFAULT 0,
    corpus_fingerprint          TEXT NOT NULL DEFAULT '',
    active_epoch_id             TEXT,
    active_epoch_corpus_generation INTEGER,
    active_epoch_completed_at_ms INTEGER,
    active_generation_contract  TEXT,
    build_epoch_id              TEXT,
    updated_at_ms               INTEGER NOT NULL DEFAULT 0
);
INSERT OR IGNORE INTO catalyst_epoch_state (singleton) VALUES (1);

-- Epoch-keyed target projections. Configured targets stage these rows before
-- the source epoch commits; split-DB reads select by the source active epoch,
-- making source activation the single logical commit point across DB files.
CREATE TABLE IF NOT EXISTS catalyst_epoch_similarities (
    epoch_id         TEXT NOT NULL,
    catalyst_id      TEXT NOT NULL,
    catalyst_text    TEXT NOT NULL,
    file_path        TEXT NOT NULL,
    chunk_index      INTEGER NOT NULL DEFAULT 0,
    similarity       REAL NOT NULL,
    chunk_metadata   TEXT DEFAULT '{}',
    timeline_data    TEXT DEFAULT '{}',
    created_at       TEXT DEFAULT (datetime('now')),
    PRIMARY KEY (epoch_id, catalyst_id, file_path, chunk_index)
);
CREATE INDEX IF NOT EXISTS idx_epoch_similarities_lookup
    ON catalyst_epoch_similarities(epoch_id, catalyst_id, similarity DESC);

CREATE TABLE IF NOT EXISTS catalyst_epoch_embeddings_packed (
    epoch_id TEXT PRIMARY KEY,
    count    INTEGER NOT NULL,
    ids      TEXT NOT NULL,
    vectors  BLOB NOT NULL
);
"#;

/// Indexes over columns that migrations add to existing tables. `SCHEMA_SQL`
/// cannot create them: on an older database its `CREATE TABLE IF NOT EXISTS`
/// keeps the old table, so the index would name a missing column and stop the
/// batch. Run after the columns are ensured.
pub const POST_MIGRATION_SQL: &str = r#"
-- Covers per-entity weight sums, which petri and selection read for every
-- entity; without weight in the index each sum reads whole occurrence rows.
CREATE INDEX IF NOT EXISTS idx_entity_occurrences_entity_weight ON entity_occurrences(entity_id, weight);
"#;

/// All tables expected in a fully-migrated database (used for validation).
/// Columns added to existing tables by migrations (see `migrations.rs`). A
/// read-only reader checks them too, since a pre-migration index has the
/// tables but would fail queries that select these columns.
pub const MIGRATED_COLUMNS: &[(&str, &str)] =
    &[("chunks", "entry_date"), ("entity_occurrences", "weight")];

pub const EXPECTED_TABLES: &[&str] = &[
    "docs",
    "doc_tags",
    "doc_links",
    "chunks",
    "entities",
    "entity_occurrences",
    "catalysts",
    "catalyst_entity_hashes",
    "catalyst_similarities",
    "context_cache",
    "config",
    "source_refreshes",
    "source_filter_cache",
    "applied_targets",
    "chunk_embeddings",
    "catalyst_embeddings_packed",
    "catalyst_epochs",
    "catalyst_epoch_state",
    "catalyst_epoch_similarities",
    "catalyst_epoch_embeddings_packed",
];

/// All indexes created by the schema (used for validation in tests).
pub const EXPECTED_INDEXES: &[&str] = &[
    "idx_doc_tags_tag",
    "idx_doc_links_link",
    "idx_entities_name",
    "idx_entities_frequency",
    "idx_catalysts_entity",
    "idx_catalysts_context",
    "idx_entity_occurrences_entity_weight",
    "idx_docs_source_modified",
    "idx_entity_occurrences_file",
    "idx_catalyst_epochs_status",
    "idx_epoch_similarities_lookup",
];
