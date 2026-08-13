//! Embedder-facing orchestration for building and querying a local search index.
//!
//! This module deliberately contains no CLI, account, hosted-service, or global
//! path resolution. Callers choose the vault and database paths, provide the
//! indexing/selection policy, and explicitly supply a generator when thematic
//! bridges are desired.

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::config::enzyme_config::EnzymeConfig;
use crate::db::Database;
use crate::embedding::{EmbeddingService, get_embedding_service};
pub use crate::llm::Generator;
use crate::models::EntityType;
use crate::pipeline::catalysts::{CatalystConfig, CatalystPipeline};
use crate::pipeline::embedding::EmbeddingPipeline;
use crate::pipeline::indexing::IndexingPipeline;
use crate::pipeline::selection::SelectionPipeline;
use crate::pipeline::similarity::SimilarityPipeline;
use crate::search::ContextSearch;

/// Caller-owned policy for indexing and selecting catalyst entities.
#[derive(Debug, Clone)]
pub struct SearchOptions {
    /// Root containing documents to index.
    pub vault_path: PathBuf,
    /// Exact database file to open or create.
    pub db_path: PathBuf,
    /// Optional caller-selected config path. It is loaded directly for index
    /// exclusions, configured logs, and catalyst selection. No default path is
    /// resolved when it is absent.
    pub config_path: Option<PathBuf>,
    /// Folders excluded from indexing. An explicit empty list means no
    /// caller-defined exclusions; no config path is resolved implicitly.
    pub excluded_folders: Vec<String>,
    /// Config-declared log entities used during indexing.
    pub configured_logs: HashSet<String>,
    /// Optional frozen selection configuration. `None` selects by coverage and
    /// does not read a vault config file.
    pub selection_config: Option<EnzymeConfig>,
    /// Force all discovered documents through indexing.
    pub full_reindex: bool,
    /// Maximum entities for which catalysts may be generated.
    pub max_bridged_entities: usize,
}

impl SearchOptions {
    pub fn new(vault_path: impl Into<PathBuf>, db_path: impl Into<PathBuf>) -> Self {
        Self {
            vault_path: vault_path.into(),
            db_path: db_path.into(),
            ..Default::default()
        }
    }

    pub fn with_max_bridged_entities(mut self, max: usize) -> Self {
        self.max_bridged_entities = max;
        self
    }
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            vault_path: PathBuf::new(),
            db_path: PathBuf::new(),
            config_path: None,
            excluded_folders: Vec::new(),
            configured_logs: HashSet::new(),
            selection_config: None,
            full_reindex: false,
            max_bridged_entities: 32,
        }
    }
}

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
    db: Database,
    service: &'static EmbeddingService,
    pub document_count: usize,
    pub bridged: Bridged,
}

impl SearchIndex {
    pub fn database(&self) -> &Database {
        &self.db
    }

    pub fn catalyst_search(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        Ok(ContextSearch::new(&self.db, self.service)
            .catalyze_search(query, limit)
            .context("catalyst search")?
            .results
            .into_iter()
            .map(|hit| Hit {
                path: hit.file_path,
                score: hit.similarity,
                content: hit.content,
            })
            .collect())
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
}

/// Open `db_path` and run index -> embed -> select -> catalyst -> similarity.
///
/// Existing sync-shaped database/pipeline operations run on a blocking worker;
/// that worker owns the runtime needed by catalyst generation. This is safe to
/// call from either current-thread or multi-thread Tokio runtimes.
pub async fn ensure_searchable(
    options: SearchOptions,
    generator: Option<Generator>,
) -> Result<SearchIndex> {
    tokio::task::spawn_blocking(move || ensure_searchable_owned(options, generator))
        .await
        .context("search provisioning worker stopped")?
}

/// Synchronous convenience wrapper that owns all runtime and thread policy.
pub fn ensure_searchable_sync(
    options: SearchOptions,
    generator: Option<Generator>,
) -> Result<SearchIndex> {
    std::thread::spawn(move || ensure_searchable_owned(options, generator))
        .join()
        .map_err(|_| anyhow::anyhow!("search provisioning worker panicked"))?
}

fn ensure_searchable_owned(
    options: SearchOptions,
    generator: Option<Generator>,
) -> Result<SearchIndex> {
    let SearchOptions {
        vault_path,
        db_path,
        config_path,
        mut excluded_folders,
        mut configured_logs,
        mut selection_config,
        full_reindex,
        max_bridged_entities,
    } = options;
    if let Some(config_path) = config_path {
        let loaded = EnzymeConfig::load_at(&vault_path, &config_path)
            .with_context(|| format!("loading search config {}", config_path.display()))?;
        if selection_config.is_none() {
            selection_config = loaded.clone();
        }
        if let Some(config) = loaded {
            if excluded_folders.is_empty() {
                excluded_folders = config.excluded_folders.clone();
            }
            if configured_logs.is_empty() {
                configured_logs = config
                    .parse_preferences()
                    .entities
                    .into_iter()
                    .filter(|entity| entity.entity_type == EntityType::AppendLog)
                    .map(|entity| entity.name)
                    .collect();
            }
        }
    }
    let db = Database::open(&db_path)
        .with_context(|| format!("opening search index {}", db_path.display()))?;
    IndexingPipeline::new(&db, vault_path.clone())
        .with_excluded_folders(excluded_folders)
        .with_configured_logs(configured_logs)
        .run(full_reindex, |_| {})
        .context("indexing documents")?;
    let document_count = db.get_document_count().unwrap_or(0) as usize;
    let service = get_embedding_service(&vault_path).context("initializing embeddings")?;
    EmbeddingPipeline::new(&db, service)
        .run(|_| {})
        .context("embedding documents")?;

    let bridged = if let Some(generator) = generator {
        let selection = SelectionPipeline::new(&db, &vault_path)
            .run_with_config(selection_config.as_ref())
            .context("selecting catalyst entities")?;
        let mut entities = selection.entities;
        entities.truncate(max_bridged_entities);
        if entities.is_empty() {
            Bridged::NoEntities
        } else {
            let config = CatalystConfig {
                model: generator.model(),
                entity_profiles: &selection.entity_profiles,
                ..Default::default()
            };
            // Catalyst/database calls use `block_in_place`; a current-thread
            // runtime panics there, most visibly for the in-process local
            // generator. Own a small multi-thread runtime so sync embedders do
            // not inherit that Tokio requirement.
            let runtime = catalyst_runtime()?;
            runtime
                .block_on(
                    CatalystPipeline::new(&db, generator.client(), Some(service)).run(
                        &entities,
                        &config,
                        |_| {},
                    ),
                )
                .context("generating catalysts")?;
            SimilarityPipeline::new(&db, service)
                .run(|_, _| {})
                .context("building catalyst similarities")?;
            Bridged::Ready
        }
    } else {
        Bridged::NoGenerator
    };

    Ok(SearchIndex {
        db,
        service,
        document_count,
        bridged,
    })
}

fn catalyst_runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("starting catalyst runtime")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let vault = temp.path().join("vault");
        std::fs::create_dir(&vault).unwrap();
        std::fs::write(
            vault.join("note.md"),
            "Rust search boundaries make embedding honest.",
        )
        .unwrap();
        let db = temp.path().join("explicit.db");
        (temp, vault, db)
    }

    #[test]
    fn sync_api_uses_explicit_path_and_builds_direct_search() {
        let (_temp, vault, db_path) = fixture();
        let index = ensure_searchable_sync(SearchOptions::new(&vault, &db_path), None).unwrap();
        assert!(db_path.exists());
        assert_eq!(index.document_count, 1);
        assert_eq!(index.bridged, Bridged::NoGenerator);
        assert!(
            !index
                .direct_search("embedding search", 5)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn async_api_works_on_current_thread_runtime() {
        let (_temp, vault, db_path) = fixture();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let index = ensure_searchable(SearchOptions::new(&vault, &db_path), None)
                    .await
                    .unwrap();
                assert_eq!(index.document_count, 1);
            });
    }

    #[test]
    fn owned_catalyst_runtime_supports_block_in_place() {
        catalyst_runtime().unwrap().block_on(async {
            assert_eq!(tokio::task::block_in_place(|| 42), 42);
        });
    }

    #[test]
    fn explicit_exclusions_are_not_read_or_indexed() {
        let (_temp, vault, db_path) = fixture();
        let excluded = vault.join("private");
        std::fs::create_dir(&excluded).unwrap();
        std::fs::write(excluded.join("secret.md"), "do not index").unwrap();
        let options = SearchOptions {
            excluded_folders: vec!["private".into()],
            ..SearchOptions::new(&vault, &db_path)
        };
        let index = ensure_searchable_sync(options, None).unwrap();
        assert_eq!(index.document_count, 1);
    }

    #[test]
    fn explicit_config_path_drives_index_policy() {
        let (_temp, vault, db_path) = fixture();
        let ignored = vault.join("ignored");
        std::fs::create_dir(&ignored).unwrap();
        std::fs::write(ignored.join("secret.md"), "excluded by injected config").unwrap();
        let config_path = vault.join("embedder.toml");
        let vault_key = vault.canonicalize().unwrap();
        std::fs::write(
            &config_path,
            format!(
                "[vaults.\"{}\"]\nexcluded_folders = [\"ignored\"]\n",
                vault_key.display()
            ),
        )
        .unwrap();

        let options = SearchOptions {
            config_path: Some(config_path),
            ..SearchOptions::new(&vault, &db_path)
        };
        let index = ensure_searchable_sync(options, None).unwrap();
        assert_eq!(index.document_count, 1);
    }
}
