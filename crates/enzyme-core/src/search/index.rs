//! Query a provisioned search index: catalyst, direct, and exact-phrase hits.
//!
//! Shared by the CLI's `enzyme search` and the embedder [`crate::kernel`], so
//! both read an index through one code path.

use std::path::Path;

use anyhow::{Context, Result};

use crate::db::Database;
use crate::embedding::{EmbeddingService, get_embedding_service};
use crate::search::{CatalyzeResponse, ContextSearch};

/// Whether thematic bridge search was provisioned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bridged {
    Ready,
    NoGenerator,
    NoEntities,
}

/// A normalized search result for embedders.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub path: String,
    pub score: f64,
    pub content: String,
}

/// A provisioned local search index.
pub struct SearchIndex {
    pub(crate) db: Database,
    pub(crate) service: &'static EmbeddingService,
    pub document_count: usize,
    pub bridged: Bridged,
}

impl SearchIndex {
    pub fn database(&self) -> &Database {
        &self.db
    }

    pub fn catalyst_search(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        Ok(self
            .catalyst_search_response(query, limit)?
            .results
            .into_iter()
            .map(|hit| Hit {
                path: hit.file_path,
                score: hit.similarity,
                content: hit.content,
            })
            .collect())
    }

    /// Catalyst search with contributing bridge metadata and per-result
    /// catalyst provenance retained for embedding applications.
    pub fn catalyst_search_response(&self, query: &str, limit: usize) -> Result<CatalyzeResponse> {
        ContextSearch::new(&self.db, self.service)
            .catalyze_search(query, limit)
            .context("catalyst search")
    }

    pub fn direct_search(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        Ok(ContextSearch::new(&self.db, self.service)
            .direct_search(query, limit)
            .context("direct embedding search")?
            .into_iter()
            .map(|hit| Hit {
                path: hit.file_path,
                score: hit.similarity,
                content: hit.content,
            })
            .collect())
    }

    /// Bounded literal phrase lookup over indexed chunks.
    ///
    /// Catalyst selection controls associative handles, not the reachability
    /// of documents already inside the declared recall boundary.
    pub fn exact_phrase_search(&self, phrase: &str, limit: usize) -> Result<Vec<Hit>> {
        Ok(self
            .db
            .get_chunks_containing_phrase(phrase, limit)?
            .into_iter()
            .map(|chunk| Hit {
                path: chunk.file_path,
                score: 1.0,
                content: chunk.content,
            })
            .collect())
    }
}

/// Open an already-provisioned index for search without provisioning it.
///
/// Returns `None` when `db_path` does not exist. The index is opened read-only
/// and never migrated or written; an index whose schema predates this binary
/// is an error rather than an in-place upgrade. `home` locates the embedding
/// model. Callers already inside a Tokio runtime should run this on a worker
/// thread (see [`crate::kernel::open_existing_sync`]).
pub fn open_read_only(home: &Path, db_path: &Path) -> Result<Option<SearchIndex>> {
    if !db_path.exists() {
        return Ok(None);
    }
    let db = Database::open_read_only(db_path)
        .with_context(|| format!("opening search index {}", db_path.display()))?;
    db.validate_expected_schema()
        .with_context(|| format!("opening search index {}", db_path.display()))?;
    let document_count = db.get_document_count().unwrap_or(0) as usize;
    let service = get_embedding_service(home).context("initializing embeddings")?;
    let bridged = if db.get_all_catalysts().map(|c| c.is_empty()).unwrap_or(true) {
        Bridged::NoGenerator
    } else {
        Bridged::Ready
    };
    Ok(Some(SearchIndex {
        db,
        service,
        document_count,
        bridged,
    }))
}
