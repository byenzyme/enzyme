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
    chunk_index INTEGER NOT NULL,
    content     TEXT    NOT NULL,
    word_count  INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL,
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
    date_source    TEXT    DEFAULT 'filesystem'
);

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
"#;

/// All tables expected in a fully-migrated database (used for validation).
pub const EXPECTED_TABLES: &[&str] = &[
    "docs",
    "doc_tags",
    "doc_links",
    "chunks",
    "entities",
    "entity_occurrences",
    "catalysts",
    "catalyst_similarities",
    "context_cache",
    "config",
];

/// All indexes created by the schema (used for validation in tests).
pub const EXPECTED_INDEXES: &[&str] = &[
    "idx_doc_tags_tag",
    "idx_doc_links_link",
    "idx_entities_name",
    "idx_entities_frequency",
    "idx_catalysts_entity",
    "idx_catalysts_context",
];
