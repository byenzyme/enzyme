//! Embedder-facing orchestration for building and querying a local search index.
//!
//! Search-index paths and selection policy remain caller-owned. Catalyst generation
//! is optional; embedders that want it call [`Generator::resolve`] so environment,
//! configured local inference, and anonymous hosted fallback share one policy.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

#[cfg(feature = "embedder-compat")]
use std::path::Path;

use anyhow::{Context, Result};

use crate::config::enzyme_config::EnzymeConfig;
use crate::db::Database;
use crate::embedding::{EmbeddingService, get_embedding_service};
pub use crate::llm::Generator;
#[cfg(feature = "embedder-compat")]
use crate::models::Embedding;
use crate::models::EntityType;
use crate::pipeline::catalysts::{CatalystConfig, CatalystPipeline};
use crate::pipeline::embedding::EmbeddingPipeline;
use crate::pipeline::epoch::{GenerationContract, now_ms};
use crate::pipeline::indexing::IndexingPipeline;
use crate::pipeline::selection::SelectionPipeline;
use crate::pipeline::similarity::SimilarityPipeline;
use crate::search::{CatalyzeResponse, ContextSearch};
use crate::sqlite_source::run_configured_sqlite_sources;

/// Caller-owned policy for indexing and selecting catalyst entities.
#[derive(Debug, Clone)]
pub struct SearchOptions {
    /// Root containing documents to index.
    pub vault_path: PathBuf,
    /// Exact database file to open or create.
    pub db_path: PathBuf,
    /// Optional caller-selected config path. It is loaded directly for index
    /// exclusions, configured logs, frontmatter link fields, and catalyst
    /// selection. No default path is resolved when it is absent.
    pub config_path: Option<PathBuf>,
    /// Folders excluded from indexing. An explicit empty list means no
    /// caller-defined exclusions; no config path is resolved implicitly.
    pub excluded_folders: Vec<String>,
    /// Config-declared log entities used during indexing.
    pub configured_logs: HashSet<String>,
    /// Plain frontmatter fields promoted to generic link entities during
    /// indexing. An explicit empty set means none when no config path is set.
    pub frontmatter_link_fields: HashSet<String>,
    /// Optional frozen selection configuration. `None` selects by coverage and
    /// does not read a vault config file.
    pub selection_config: Option<EnzymeConfig>,
    /// Force all discovered documents through indexing.
    pub full_reindex: bool,
    /// Maximum entities for which catalysts may be generated.
    pub max_bridged_entities: usize,
    /// Minimum distinct source documents in an entity's selected context.
    pub min_catalyst_context_documents: usize,
    /// Minimum substantive tokens in an entity's selected context.
    pub min_catalyst_context_tokens: usize,
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
            frontmatter_link_fields: HashSet::new(),
            selection_config: None,
            full_reindex: false,
            max_bridged_entities: 32,
            min_catalyst_context_documents:
                crate::pipeline::catalysts::DEFAULT_MIN_CONTEXT_DOCUMENTS,
            min_catalyst_context_tokens: crate::pipeline::catalysts::DEFAULT_MIN_CONTEXT_TOKENS,
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

    /// Catalyst-based search rendered as the legacy `catalyze` JSON envelope.
    ///
    /// This compatibility surface lets embedders that previously parsed the
    /// Enzyme CLI's stdout consume an in-process value without changing their
    /// envelope parser.
    #[cfg(feature = "embedder-compat")]
    pub fn catalyze_json(&self, query: &str, limit: usize) -> Result<String> {
        let response = ContextSearch::new(&self.db, self.service)
            .catalyze_search(query, limit)
            .context("catalyze search")?;
        let total = response.results.len();
        let mut output =
            serde_json::to_value(&response).context("serializing catalyze response")?;
        if let Some(obj) = output.as_object_mut() {
            obj.insert("query".into(), serde_json::json!(query));
            obj.insert("search_strategy".into(), serde_json::json!("catalyze"));
            // Real elapsed time is irrelevant to compatibility consumers, but
            // the legacy envelope always carries the field.
            obj.insert("processing_time".into(), serde_json::json!(0.0));
            obj.insert("total_results".into(), serde_json::json!(total));
        }
        Ok(serde_json::to_string(&output)?)
    }

    /// Query-aware entity ranking rendered as the legacy `petri` JSON envelope.
    #[cfg(feature = "embedder-compat")]
    pub fn petri_json(&self, query: &str, top: usize, catalyst_budget: usize) -> Result<String> {
        let query_vec = self.service.embed_query(query)?;
        let catalysts = self.db.get_all_catalysts()?;
        let cached = self.db.get_all_catalyst_embeddings().unwrap_or_default();
        let cache: HashMap<&str, &Vec<f32>> =
            cached.iter().map(|(id, v)| (id.as_str(), v)).collect();

        // Per-catalyst cosine similarity, grouped by entity (by catalyst index).
        let mut groups: HashMap<String, Vec<(usize, f32)>> = HashMap::new();
        for (i, catalyst) in catalysts.iter().enumerate() {
            if let Some(vec) = cache.get(catalyst.id.as_str()) {
                let sim = Embedding::cosine_similarity(&query_vec, vec);
                groups
                    .entry(catalyst.entity.clone())
                    .or_default()
                    .push((i, sim));
            }
        }

        let entities = self
            .db
            .get_top_entities(groups.len().max(100), "recency_score")
            .unwrap_or_default();
        let entity_by_name: HashMap<&str, &crate::models::Entity> =
            entities.iter().map(|e| (e.name.as_str(), e)).collect();
        let max_recency = entities
            .iter()
            .map(|e| e.trends.recency_score)
            .fold(0.0f64, f64::max);

        // Blend max catalyst similarity (0.7) with normalized recency (0.3).
        let mut scored: Vec<(String, f64)> = groups
            .iter()
            .filter_map(|(name, sims)| {
                let entity = entity_by_name.get(name.as_str())?;
                let max_sim = sims.iter().map(|(_, s)| *s).fold(0.0f32, f32::max) as f64;
                let norm_recency = if max_recency > 0.0 {
                    entity.trends.recency_score / max_recency
                } else {
                    0.0
                };
                Some((name.clone(), max_sim * 0.7 + norm_recency * 0.3))
            })
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(top);

        let mut entities_json = Vec::new();
        for (name, _) in &scored {
            let Some(entity) = entity_by_name.get(name.as_str()).copied() else {
                continue;
            };
            let mut group = groups.remove(name.as_str()).unwrap_or_default();
            group.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            group.truncate(catalyst_budget);
            let cats: Vec<serde_json::Value> = group
                .iter()
                .map(|&(idx, _)| {
                    serde_json::json!({
                        "text": catalysts[idx].text,
                        "context": catalysts[idx].context,
                    })
                })
                .collect();
            entities_json.push(serde_json::json!({
                "name": entity.name,
                "type": entity.entity_type.as_str(),
                "catalysts": cats,
            }));
        }

        Ok(serde_json::to_string(&serde_json::json!({
            "entities": entities_json,
            "total_entities": entities_json.len(),
            "query": query,
        }))?)
    }
}

/// Open an already-provisioned index for search without running provisioning.
///
/// Returns `None` when the index file does not exist yet so the embedder can
/// choose whether to cold-provision. The worker owns initialization and keeps
/// the sync API safe in callers that already have an async runtime.
#[cfg(feature = "embedder-compat")]
pub fn open_existing_sync(vault_path: &Path, db_path: &Path) -> Result<Option<SearchIndex>> {
    if !db_path.exists() {
        return Ok(None);
    }
    let vault = vault_path.to_path_buf();
    let db_path = db_path.to_path_buf();
    std::thread::spawn(move || -> Result<SearchIndex> {
        let db = Database::open(&db_path)
            .with_context(|| format!("opening search index {}", db_path.display()))?;
        let document_count = db.get_document_count().unwrap_or(0) as usize;
        let service = get_embedding_service(&vault).context("initializing embeddings")?;
        let bridged = if db.get_all_catalysts().map(|c| c.is_empty()).unwrap_or(true) {
            Bridged::NoGenerator
        } else {
            Bridged::Ready
        };
        Ok(SearchIndex {
            db,
            service,
            document_count,
            bridged,
        })
    })
    .join()
    .map_err(|_| anyhow::anyhow!("search open worker panicked"))?
    .map(Some)
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
        mut frontmatter_link_fields,
        mut selection_config,
        full_reindex,
        max_bridged_entities,
        min_catalyst_context_documents,
        min_catalyst_context_tokens,
    } = options;
    let mut configured_sources = None;
    let mut configured_notes_roots = Vec::new();
    if let Some(config_path) = config_path {
        configured_sources = Some(HashMap::new());
        let loaded = EnzymeConfig::load_at(&vault_path, &config_path)
            .with_context(|| format!("loading search config {}", config_path.display()))?;
        if selection_config.is_none() {
            selection_config = loaded.clone();
        }
        if let Some(config) = loaded {
            configured_sources = Some(config.sources.clone());
            configured_notes_roots = config.notes_roots.clone();
            if frontmatter_link_fields.is_empty() {
                frontmatter_link_fields = config.frontmatter_link_fields.iter().cloned().collect();
            }
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
    if let Some(sources) = configured_sources {
        run_configured_sqlite_sources(&db, &sources)
            .context("indexing configured SQLite sources")?;
    }
    let mut indexing = IndexingPipeline::new(&db, vault_path.clone())
        .with_excluded_folders(excluded_folders)
        .with_configured_logs(configured_logs)
        .with_frontmatter_link_fields(frontmatter_link_fields);
    if !configured_notes_roots.is_empty() {
        indexing = indexing.with_notes_roots(configured_notes_roots);
    }
    if let Some(config) = &selection_config {
        indexing = indexing.with_as_of(
            config
                .reading_spec
                .as_ref()
                .and_then(|spec| spec.learning.as_of.clone()),
        );
    }
    indexing
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
            let model = generator.model();
            let selection_config_json = serde_json::to_string(
                selection_config
                    .as_ref()
                    .unwrap_or(&EnzymeConfig::default()),
            )?;
            let contract = GenerationContract {
                schema_version: "catalyst-epoch.v2".into(),
                prompt_version: "catalyst-prompts.2026-09-titled-excerpt-labels".into(),
                catalyst_format: crate::llm::DEFAULT_FORMAT_KEY.into(),
                catalyst_profile: crate::llm::DEFAULT_PROFILE_KEY.into(),
                model: model.clone(),
                llm_mode: "embedder-pull".into(),
                llm_provider: format!("{:?}", generator.provider()).to_lowercase(),
                llm_base_url: "resolved-by-generator".into(),
                allocation_semantics: "volume-floor-and-top-boost.v1".into(),
                catalysts_per_entity: 7,
                min_top_catalysts: None,
                embedding_backend: service.backend_name().into(),
                embedding_dimension: service.dimension(),
                targets: Vec::new(),
                selection_config_json,
                max_context_chars: None,
                max_context_samples: 20,
                min_context_documents: min_catalyst_context_documents,
                min_context_tokens: min_catalyst_context_tokens,
            };
            let epoch_id = db.begin_inline_catalyst_epoch(&contract, now_ms())?;
            let config = CatalystConfig {
                reading_spec: selection_config.as_ref().and_then(|c| c.reading_spec.as_ref()),
                model: &model,
                entity_profiles: &selection.entity_profiles,
                min_context_documents: min_catalyst_context_documents,
                min_context_tokens: min_catalyst_context_tokens,
                epoch_id: Some(&epoch_id),
                ..Default::default()
            };
            // Catalyst/database calls use `block_in_place`; a current-thread
            // runtime panics there, most visibly for the in-process local
            // generator. Own a small multi-thread runtime so sync embedders do
            // not inherit that Tokio requirement.
            let runtime = catalyst_runtime()?;
            let generation = runtime
                .block_on(
                    CatalystPipeline::new(&db, generator.client(), Some(service)).run(
                        &entities,
                        &config,
                        |_| {},
                    ),
                )
                .context("generating catalysts");
            if let Err(error) = generation {
                db.mark_epoch_failed(&epoch_id, now_ms(), &error.to_string())?;
                return Err(error);
            }
            let similarities = SimilarityPipeline::new(&db, service)
                .run(|_, _| {})
                .context("building catalyst similarities");
            if let Err(error) = similarities {
                db.mark_epoch_failed(&epoch_id, now_ms(), &error.to_string())?;
                return Err(error);
            }
            if let Err(error) = db.activate_inline_catalyst_epoch(&epoch_id, now_ms()) {
                db.mark_epoch_failed(&epoch_id, now_ms(), &error.to_string())?;
                return Err(error);
            }
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
    use crate::config::enzyme_config::CatalyzedEntity;
    use crate::llm::LlmClient;
    use crate::models::parse_catalyst_text;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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

    #[cfg(feature = "embedder-compat")]
    fn empty_index() -> SearchIndex {
        let db = Database::open_in_memory().expect("in-memory db");
        let service = get_embedding_service(std::path::Path::new(".")).expect("embeddings");
        SearchIndex {
            db,
            service,
            document_count: 0,
            bridged: Bridged::NoGenerator,
        }
    }

    #[cfg(feature = "embedder-compat")]
    #[test]
    fn catalyze_json_envelope_shape() {
        let index = empty_index();
        let value: serde_json::Value =
            serde_json::from_str(&index.catalyze_json("a query", 5).unwrap()).unwrap();
        assert!(value["results"].is_array());
        assert!(value["top_contributing_catalysts"].is_array());
        assert_eq!(value["query"], "a query");
        assert_eq!(value["search_strategy"], "catalyze");
        assert!(value["processing_time"].is_number());
        assert_eq!(value["total_results"], 0);
    }

    #[cfg(feature = "embedder-compat")]
    #[test]
    fn petri_json_envelope_shape() {
        let index = empty_index();
        let value: serde_json::Value =
            serde_json::from_str(&index.petri_json("a query", 8, 2).unwrap()).unwrap();
        assert!(value["entities"].is_array());
        assert_eq!(value["total_entities"], 0);
        assert_eq!(value["query"], "a query");
    }

    #[cfg(feature = "embedder-compat")]
    #[test]
    fn open_existing_returns_none_for_missing_index() {
        let temp = tempfile::tempdir().unwrap();
        let vault = temp.path().join("vault");
        std::fs::create_dir(&vault).unwrap();
        let result = open_existing_sync(&vault, &temp.path().join("missing.db")).unwrap();
        assert!(result.is_none());
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn provision_path_persists_epoch_era_and_source_refs() {
        let temp = tempfile::tempdir().unwrap();
        let vault = temp.path().join("vault");
        std::fs::create_dir(&vault).unwrap();
        for index in 0..4 {
            std::fs::write(
                vault.join(format!("evidence-{index}.md")),
                format!(
                    "#focus\n\nDocument {index}: {}",
                    "The concrete source migration preserves auditable evidence and occurrence provenance. "
                        .repeat(25)
                ),
            )
            .unwrap();
        }

        let server = MockServer::start().await;
        let content = serde_json::json!({
            "catalysts": ["Could preserving occurrence provenance change the migration decision?"],
            "topicNames": ["auditable migration provenance"]
        });
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": content.to_string()}, "finish_reason": "stop"}]
            })))
            .mount(&server)
            .await;

        let selection = EnzymeConfig {
            catalyzed_entities: vec![CatalyzedEntity::Simple("#focus".to_string())],
            ..Default::default()
        };
        let options = SearchOptions {
            selection_config: Some(selection),
            full_reindex: true,
            max_bridged_entities: 1,
            ..SearchOptions::new(&vault, temp.path().join("provision.db"))
        };
        let generator = Generator::fixture(
            LlmClient::with_base_url("fixture-key", &server.uri()),
            "fixture-model",
        );
        let index = ensure_searchable_sync(options, Some(generator)).unwrap();
        assert_eq!(index.bridged, Bridged::Ready);

        let epochs = index
            .database()
            .query_read(
                "SELECT epoch_id FROM catalyst_epochs WHERE status='active'",
                vec![],
                |row| row.get::<String>(0),
            )
            .unwrap();
        assert_eq!(epochs.len(), 1);
        let catalyst = index.database().get_all_catalysts().unwrap().pop().unwrap();
        let canonical = parse_catalyst_text(&catalyst.text).unwrap();
        assert!(
            catalyst
                .metadata
                .get("era")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|era| !era.is_empty())
        );
        assert!(!canonical.source_refs.is_empty());
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
        std::fs::write(
            vault.join("note.md"),
            "---\npeople: [Jay Vidyarthi]\n---\nRust search boundaries make embedding honest.",
        )
        .unwrap();
        let ignored = vault.join("ignored");
        std::fs::create_dir(&ignored).unwrap();
        std::fs::write(ignored.join("secret.md"), "excluded by injected config").unwrap();
        let config_path = vault.join("embedder.toml");
        let vault_key = vault.canonicalize().unwrap();
        std::fs::write(
            &config_path,
            format!(
                "[vaults.\"{}\"]\nexcluded_folders = [\"ignored\"]\nfrontmatter_link_fields = [\"people\"]\n",
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
        assert!(
            index
                .database()
                .get_all_entities()
                .unwrap()
                .iter()
                .any(|entity| {
                    entity.name == "jay vidyarthi" && entity.entity_type == EntityType::Link
                })
        );
    }
}
