use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use enzyme_core::config::paths::{EnzymePaths, ensure_dir_symlink_aware};
use enzyme_core::db::Database;
use enzyme_core::document::{
    ProcessedDocument, StructuredDocumentInput, structured_document_to_processed_document,
};
use enzyme_core::embedding::get_embedding_service;
use enzyme_core::models::{EntityOccurrence, EntityType, representative_excerpt};
use enzyme_core::pipeline::epoch::now_ms as epoch_now_ms;
use enzyme_spec::{ArenaChannel, ArenaSource, Learning, Program, Reading, Source, Workspace};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const USER_AGENT: &str = concat!("Enzyme/", env!("CARGO_PKG_VERSION"), " Are.na integration");
const ARENA_SOURCE_NAME: &str = "channels";
const MAX_LINKS_PER_BLOCK: usize = 3;
const MAX_LINK_FETCHES: usize = 24;
const MAX_LINKED_TEXT_CHARS: usize = 3_000;

#[derive(Debug, Clone)]
struct LinkedText {
    id: i64,
    title: String,
    body: String,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum SamplingMode {
    /// Recency-weighted evidence with period coverage when capacity permits.
    Recency,
    /// Balance evidence and questions across represented periods.
    Time,
}

impl SamplingMode {
    fn learning(self, budget: usize) -> Learning {
        Learning {
            budget: Some(budget),
            sample: Some("across_time".into()),
            favor: Some(
                match self {
                    Self::Recency => "recent",
                    Self::Time => "equal",
                }
                .into(),
            ),
            ..Default::default()
        }
    }

    fn canonical(self) -> &'static str {
        match self {
            Self::Recency => "sample by recency",
            Self::Time => "sample by time",
        }
    }
}

#[derive(Debug, Deserialize)]
struct Counts {
    contents: usize,
}

#[derive(Debug, Deserialize)]
struct Channel {
    id: i64,
    slug: String,
    title: String,
    counts: Counts,
}

#[derive(Debug, Deserialize)]
struct PageMeta {
    total_count: usize,
}

#[derive(Debug, Deserialize)]
struct ContentsPage {
    data: Vec<Value>,
    meta: PageMeta,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChannelCoverage {
    pub channel_id: i64,
    pub slug: String,
    pub title: String,
    pub entity: String,
    pub reported_connections: usize,
    pub fetched_connections: usize,
    pub indexed_documents: usize,
    pub excluded_images: usize,
    pub nested_channels: usize,
    pub oldest_connected_at: Option<String>,
    pub newest_connected_at: Option<String>,
    pub connected_by_month: BTreeMap<String, usize>,
    pub partial: bool,
}

#[derive(Debug, Serialize)]
struct CoverageReceipt<'a> {
    source: &'static str,
    fetched_at: String,
    api_fetch: ApiFetchReceipt,
    catalyst_evidence: CatalystEvidenceReceipt,
    channels: &'a [ChannelCoverage],
}

#[derive(Debug, Serialize)]
struct ApiFetchReceipt {
    scope: &'static str,
    requested_per_channel: usize,
    sort: &'static str,
    ordering_evidence: &'static str,
    distribution_scope: &'static str,
    deletion_reconciliation: bool,
}

#[derive(Debug, Serialize)]
struct CatalystEvidenceReceipt {
    policy: String,
    explanation: &'static str,
    plan_command: String,
    plan_argv: Vec<String>,
    planned_distribution: &'static str,
    completed_epoch_evidence: &'static str,
}

#[derive(Debug)]
struct NormalizedChannel {
    documents: Vec<ProcessedDocument>,
    occurrences: Vec<(String, EntityOccurrence)>,
    coverage: ChannelCoverage,
}

#[derive(Debug)]
struct DeclaredChannelScope {
    selectors: BTreeMap<String, String>,
    remote_ids: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ArenaChannelSummary {
    pub id: i64,
    pub title: String,
    pub reported_connections: usize,
}

/// Resolve channel selectors into a stable, editable source declaration.
/// Compilation fetches channel metadata only; init and refresh import content
/// from the saved source declaration.
pub(crate) async fn compile_preset_program(
    workspace_name: &str,
    inputs: &[String],
    api_base: &str,
) -> Result<(Program, Vec<ArenaChannelSummary>)> {
    ensure!(
        !inputs.is_empty(),
        "the Are.na preset needs at least one channel URL, slug, or ID"
    );
    let base = normalized_api_base(api_base)?;
    let client = Client::builder()
        .user_agent(USER_AGENT)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("build Are.na HTTP client")?;
    let token = if base.scheme() == "https" && base.host_str() == Some("api.are.na") {
        std::env::var("ARENA_ACCESS_TOKEN")
            .ok()
            .filter(|value| !value.trim().is_empty())
    } else {
        None
    };
    let mut seen = BTreeSet::new();
    let mut normalized = Vec::new();
    let mut summaries = Vec::new();
    for input in inputs {
        let identifier = channel_identifier(input)?;
        let channel: Channel = get_json(
            &client,
            &base,
            &["channels", &identifier],
            &[],
            token.as_deref(),
        )
        .await
        .with_context(|| format!("resolve Are.na channel {identifier}"))?;
        if !seen.insert(channel.id) {
            continue;
        }
        summaries.push(ArenaChannelSummary {
            id: channel.id,
            title: channel.title.clone(),
            reported_connections: channel.counts.contents,
        });
        normalized.push(NormalizedChannel {
            coverage: ChannelCoverage {
                channel_id: channel.id,
                slug: channel.slug,
                title: channel.title,
                entity: collection_key(ARENA_SOURCE_NAME, channel.id),
                reported_connections: channel.counts.contents,
                fetched_connections: 0,
                indexed_documents: 0,
                excluded_images: 0,
                nested_channels: 0,
                oldest_connected_at: None,
                newest_connected_at: None,
                connected_by_month: BTreeMap::new(),
                partial: false,
            },
            documents: Vec::new(),
            occurrences: Vec::new(),
        });
    }
    Ok((
        build_program(workspace_name, &normalized, SamplingMode::Recency, 15),
        summaries,
    ))
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    vault_path: &Path,
    workspace_name: Option<&str>,
    channel_inputs: &[String],
    limit: usize,
    sampling: SamplingMode,
    question_budget: usize,
    dry_run: bool,
    index_only: bool,
    json: bool,
    api_base: &str,
    selected_program_path: Option<&Path>,
    quiet: bool,
) -> Result<()> {
    let workspace_name = workspace_name.context("Are.na source requires a named workspace")?;
    ensure!(
        (1..=100).contains(&limit),
        "--limit must be between 1 and 100"
    );
    ensure!(question_budget >= 3, "--question-budget must be at least 3");

    std::fs::create_dir_all(vault_path)
        .with_context(|| format!("create vault directory {}", vault_path.display()))?;
    let vault_path = vault_path
        .canonicalize()
        .with_context(|| format!("resolve vault directory {}", vault_path.display()))?;
    let paths = EnzymePaths::new(&vault_path)?;
    let program_path = selected_program_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| arena_program_path(&paths, workspace_name));
    let existing_program = program_path
        .exists()
        .then(|| {
            std::fs::read_to_string(&program_path)
                .with_context(|| format!("read existing {}", program_path.display()))
        })
        .transpose()?;
    let declared_scope = existing_program
        .as_deref()
        .map(|source| declared_channel_scope(source, workspace_name, &program_path))
        .transpose()?;
    let requested_channels = if channel_inputs.is_empty() {
        declared_scope
            .as_ref()
            .map(|scope| scope.remote_ids.clone())
            .context(
                "name at least one Are.na channel on the first import; later imports may omit channels to use the saved source scope",
            )?
    } else {
        channel_inputs.to_vec()
    };
    let base = normalized_api_base(api_base)?;
    let client = Client::builder()
        .user_agent(USER_AGENT)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("build Are.na HTTP client")?;
    let token = if base.scheme() == "https" && base.host_str() == Some("api.are.na") {
        std::env::var("ARENA_ACCESS_TOKEN")
            .ok()
            .filter(|value| !value.trim().is_empty())
    } else {
        None
    };

    let mut normalized = Vec::new();
    let mut seen = BTreeSet::new();
    let mut seen_remote_ids = BTreeSet::new();
    let mut linked_cache = BTreeMap::new();
    let mut link_fetches = 0;
    for input in &requested_channels {
        let identifier = channel_identifier(input)?;
        if !seen.insert(identifier.clone()) {
            continue;
        }
        let expected_remote_id = declared_scope
            .as_ref()
            .and_then(|scope| scope.selectors.get(&identifier.to_lowercase()).cloned());
        let lookup_identifier = expected_remote_id.as_deref().unwrap_or(&identifier);
        let channel: Channel = get_json(
            &client,
            &base,
            &["channels", lookup_identifier],
            &[],
            token.as_deref(),
        )
        .await
        .with_context(|| format!("fetch Are.na channel {identifier}"))?;
        if let Some(scope) = &declared_scope {
            ensure!(
                scope
                    .remote_ids
                    .iter()
                    .any(|id| id == &channel.id.to_string()),
                "Are.na resolved {identifier:?} to channel {}, which is outside the source arena scope in {}",
                channel.id,
                program_path.display()
            );
        }
        if !seen_remote_ids.insert(channel.id) {
            continue;
        }
        let page: ContentsPage = get_json(
            &client,
            &base,
            &["channels", &channel.id.to_string(), "contents"],
            &[
                ("page", "1".into()),
                ("per", limit.to_string()),
                ("sort", "created_at_desc".into()),
            ],
            token.as_deref(),
        )
        .await
        .with_context(|| format!("fetch bounded contents for {}", channel.title))?;
        let linked = hydrate_linked_text(
            &client,
            &base,
            &page,
            token.as_deref(),
            &mut linked_cache,
            &mut link_fetches,
        )
        .await;
        normalized.push(normalize_channel(
            ARENA_SOURCE_NAME,
            channel,
            page,
            &linked,
        )?);
    }

    let coverages: Vec<_> = normalized
        .iter()
        .map(|item| item.coverage.clone())
        .collect();
    let program = build_program(workspace_name, &normalized, sampling, question_budget);
    let program_text = render_program_with_receipt(&program, &coverages, sampling);
    let plan_argv = vec![
        "enzyme".to_string(),
        "-p".to_string(),
        vault_path.to_string_lossy().into_owned(),
        "spec".to_string(),
        "plan".to_string(),
        program_path.to_string_lossy().into_owned(),
        "--json".to_string(),
    ];
    let plan_command = plan_argv
        .iter()
        .map(|part| shell_quote(part))
        .collect::<Vec<_>>()
        .join(" ");
    let mut receipt = CoverageReceipt {
        source: "arena",
        fetched_at: Utc::now().to_rfc3339(),
        api_fetch: ApiFetchReceipt {
            scope: "declared channel scope, one bounded page each",
            requested_per_channel: limit,
            sort: "created_at_desc",
            ordering_evidence: "Are.na v3 sort name plus a public-channel check support descending connection creation; observed connected_at bounds are reported per channel.",
            distribution_scope: "counts describe only the fetched window, never the full channel",
            deletion_reconciliation: false,
        },
        catalyst_evidence: CatalystEvidenceReceipt {
            policy: sampling.canonical().to_string(),
            explanation: "API fetch coverage is separate. spec plan uses the current index and generated program to compute the same pre-generation job shape used by catalyst generation.",
            plan_command: plan_command.clone(),
            plan_argv,
            planned_distribution: "available read-only from spec plan as planned excerpts and requested questions by period",
            completed_epoch_evidence: "not captured by this import receipt; a plan describes intended prompt evidence before generation, not what a completed epoch ultimately used or produced",
        },
        channels: &coverages,
    };

    if dry_run {
        let displayed_program = if let Some(existing) = existing_program.as_deref() {
            receipt.catalyst_evidence.policy =
                validate_existing_program(existing, workspace_name, &coverages, &program_path)?;
            existing
        } else {
            &program_text
        };
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "program": displayed_program,
                    "program_path": program_path,
                    "coverage": receipt,
                    "dry_run": true,
                }))?
            );
        } else {
            print!("{displayed_program}");
            print_coverage(&coverages, &receipt.catalyst_evidence.policy, &plan_command);
        }
        return Ok(());
    }

    if let Some(parent) = paths.vault_db.parent() {
        ensure_dir_symlink_aware(parent).context("create Enzyme index directory")?;
    }
    if let Some(parent) = program_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let program_preserved = if let Some(existing) = existing_program {
        receipt.catalyst_evidence.policy =
            validate_existing_program(&existing, workspace_name, &coverages, &program_path)?;
        true
    } else {
        enzyme_spec::atomic_write(&program_path, &program_text)?;
        false
    };

    let receipt_path = paths
        .vault_db
        .parent()
        .context("Enzyme database has no parent directory")?
        .join("arena-coverage.json");
    enzyme_spec::atomic_write(
        &receipt_path,
        &format!("{}\n", serde_json::to_string_pretty(&receipt)?),
    )?;

    let mut documents = BTreeMap::<String, ProcessedDocument>::new();
    let mut occurrence_entries = Vec::<(String, EntityType, EntityOccurrence)>::new();
    for item in normalized {
        for document in item.documents {
            documents.insert(document.source_ref.clone(), document);
        }
        for (entity, occurrence) in item.occurrences {
            occurrence_entries.push((entity, EntityType::Collection, occurrence));
        }
    }
    let documents: Vec<_> = documents.into_values().collect();

    let db = Database::open(&paths.vault_db)?;
    let _sync_guard = paths.sync_on_drop();
    // Earlier imports indexed Image blocks. Remove their documents and
    // occurrences so reimporting into an existing workspace applies the same
    // text-first source policy as a fresh import.
    let arena_refs = db
        .get_all_source_refs()?
        .into_iter()
        .filter(|source_ref| source_ref.starts_with("arena:block:"))
        .collect::<Vec<_>>();
    let arena_contents = db.get_doc_contents_for_paths(&arena_refs)?;
    let old_image_refs = arena_contents
        .into_iter()
        .filter_map(|(source_ref, content)| is_image_document(&content).then_some(source_ref))
        .collect::<Vec<_>>();
    for batch in old_image_refs.chunks(500) {
        db.delete_documents_by_paths(batch)?;
    }
    let imported_paths: Vec<_> = documents
        .iter()
        .map(|document| document.source_ref.clone())
        .collect();
    let preserved = db
        .get_collection_occurrences_for_paths_by_date_source(&imported_paths, "arena_connection")?;
    let indexed_connections = occurrence_entries.len();
    let observed_keys: BTreeSet<_> = occurrence_entries
        .iter()
        .map(|(name, _, occurrence)| (name.clone(), occurrence.file_path.clone()))
        .collect();
    let retained_memberships = preserved
        .iter()
        .filter(|(name, occurrence)| {
            !observed_keys.contains(&(name.clone(), occurrence.file_path.clone()))
        })
        .count();
    let service = get_embedding_service(&paths.home)?;
    let (embedding, _similarity) =
        super::ingest::project_ingest_batch(&db, &paths, &documents, service)?;
    let mut merged_occurrences = BTreeMap::new();
    for (name, occurrence) in preserved {
        merged_occurrences.insert((name, occurrence.file_path.clone()), occurrence);
    }
    for (name, _, occurrence) in occurrence_entries {
        merged_occurrences.insert((name, occurrence.file_path.clone()), occurrence);
    }
    let occurrence_entries: Vec<_> = merged_occurrences
        .into_iter()
        .map(|((name, _), occurrence)| (name, EntityType::Collection, occurrence))
        .collect();
    let occurrence_refs: Vec<_> = occurrence_entries
        .iter()
        .map(|(name, kind, occurrence)| (name.as_str(), kind, occurrence))
        .collect();
    db.upsert_entity_occurrences_partial(&occurrence_refs)?;
    db.reconcile_corpus_generation(epoch_now_ms())?;

    let (catalyst_epoch_scheduled, catalyst_generation_due, background_spawned) = if index_only {
        (false, false, false)
    } else {
        let contract =
            super::init::generation_contract(&vault_path, &paths, super::generator::LlmMode::Auto)
                .await?;
        db.bootstrap_legacy_epoch(&contract, epoch_now_ms(), false)?;
        super::init::recover_abandoned_epoch_worker(&db, &paths)?;
        let schedule = db.schedule_catalyst_epoch(epoch_now_ms(), &contract, false)?;
        (
            schedule.scheduled,
            schedule.due,
            schedule.launch_needed
                && super::init::spawn_background_refresh(
                    &vault_path,
                    &paths,
                    super::generator::LlmMode::Auto,
                ),
        )
    };

    if quiet {
        tracing::info!(
            documents = documents.len(),
            channels = coverages.len(),
            "Are.na source indexed"
        );
    } else if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "status": "ok",
                "program": program_path,
                "program_preserved": program_preserved,
                "coverage_receipt": receipt_path,
                "documents_indexed": documents.len(),
                "connections_indexed": indexed_connections,
                "images_excluded_from_window": coverages.iter().map(|channel| channel.excluded_images).sum::<usize>(),
                "previous_image_documents_removed": old_image_refs.len(),
                "memberships_retained": retained_memberships,
                "occurrences_indexed": occurrence_entries.len(),
                "chunks_embedded": embedding.chunks_embedded,
                "catalyst_epoch_scheduled": catalyst_epoch_scheduled,
                "background_spawned": background_spawned,
                "channels": coverages,
            }))?
        );
    } else {
        println!("Are.na program: {}", program_path.display());
        if program_preserved {
            println!("Preserved existing program and its catalyst tweaks.");
        }
        println!("Coverage receipt: {}", receipt_path.display());
        println!(
            "Indexed {} documents and {} eligible channel connections ({} chunks embedded).",
            documents.len(),
            indexed_connections,
            embedding.chunks_embedded
        );
        println!(
            "Excluded {} image connections; removed {} image documents from earlier imports.",
            coverages
                .iter()
                .map(|channel| channel.excluded_images)
                .sum::<usize>(),
            old_image_refs.len()
        );
        if retained_memberships > 0 {
            println!(
                "Retained {retained_memberships} previously observed memberships for shared blocks."
            );
        }
        print_coverage(&coverages, &receipt.catalyst_evidence.policy, &plan_command);
        if index_only {
            println!(
                "Catalyst generation deferred; run `enzyme -p {} refresh --full` after reviewing the program.",
                vault_path.display()
            );
        } else if background_spawned {
            println!("Catalyst generation is running in the background.");
        } else if catalyst_generation_due {
            println!(
                "Catalyst generation is due; run `enzyme -p {} refresh --full`.",
                vault_path.display()
            );
        }
    }
    Ok(())
}

/// Refresh the Are.na source declared by a named workspace program, if present.
pub async fn index_configured_source(vault_path: &Path) -> Result<()> {
    let Some(workspace_name) = EnzymePaths::bound_workspace(vault_path)? else {
        return Ok(());
    };
    let paths = EnzymePaths::new(vault_path)?;
    let directory = paths.home.join("configs");
    if !directory.exists() {
        return Ok(());
    }
    let mut found = None;
    for entry in
        std::fs::read_dir(&directory).with_context(|| format!("read {}", directory.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "enzyme") {
            continue;
        }
        let source =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        // Loading the workspace's config already failed if its own file
        // does not parse; another workspace's file must not fail this one.
        let Ok(program) = enzyme_spec::parse(&source) else {
            continue;
        };
        let matches = program.workspaces.iter().any(|workspace| {
            workspace.name == workspace_name
                && workspace
                    .sources
                    .iter()
                    .any(|source| matches!(source, Source::Arena(_)))
        });
        if matches {
            ensure!(
                found.is_none(),
                "multiple Are.na programs define workspace {workspace_name}"
            );
            found = Some(path);
        }
    }
    if let Some(program_path) = found {
        let api_base = std::env::var("ENZYME_ARENA_API_BASE")
            .unwrap_or_else(|_| "https://api.are.na/v3/".into());
        run(
            vault_path,
            Some(&workspace_name),
            &[],
            24,
            SamplingMode::Recency,
            15,
            false,
            true,
            false,
            &api_base,
            Some(&program_path),
            true,
        )
        .await?;
    }
    Ok(())
}

fn normalized_api_base(raw: &str) -> Result<Url> {
    let mut base = Url::parse(raw).context("invalid Are.na API base URL")?;
    ensure!(
        matches!(base.scheme(), "https" | "http"),
        "Are.na API base must use https or http"
    );
    if !base.path().ends_with('/') {
        let path = format!("{}/", base.path());
        base.set_path(&path);
    }
    Ok(base)
}

async fn get_json<T: for<'de> Deserialize<'de>>(
    client: &Client,
    base: &Url,
    segments: &[&str],
    query: &[(&str, String)],
    token: Option<&str>,
) -> Result<T> {
    let mut url = base.clone();
    {
        let mut path = url
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("Are.na API base cannot contain path segments"))?;
        path.pop_if_empty();
        for segment in segments {
            path.push(segment);
        }
    }
    let mut request = client
        .get(url)
        .query(query)
        .header("Accept", "application/json");
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.context("send Are.na request")?;
    let status = response.status();
    if !status.is_success() {
        bail!("Are.na returned HTTP {status}");
    }
    response.json().await.context("decode Are.na response")
}

fn channel_identifier(input: &str) -> Result<String> {
    let trimmed = input.trim().trim_end_matches('/');
    ensure!(!trimmed.is_empty(), "channel identifier cannot be empty");
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        let url = Url::parse(trimmed).context("invalid Are.na channel URL")?;
        ensure!(
            url.host_str()
                .is_some_and(|host| host == "are.na" || host == "www.are.na"),
            "channel URL must be on are.na"
        );
        let identifier = url
            .path_segments()
            .and_then(|segments| segments.filter(|part| !part.is_empty()).next_back())
            .context("Are.na channel URL has no slug or ID")?;
        return Ok(identifier.to_string());
    }
    ensure!(
        !trimmed.contains('/') && !trimmed.contains('?') && !trimmed.contains('#'),
        "channel must be an Are.na URL, slug, or numeric ID"
    );
    Ok(trimmed.to_string())
}

fn collection_key(source_name: &str, channel_id: i64) -> String {
    format!("arena:{source_name}/channel/{channel_id}")
}

fn is_image_document(content: &str) -> bool {
    content
        .lines()
        .find(|line| line.starts_with("Type: "))
        .is_some_and(|line| line == "Type: Image")
}

fn linked_block_ids(text: &str) -> Vec<i64> {
    let mut seen = BTreeSet::new();
    let mut ids = Vec::new();
    for (start, _) in text.match_indices("http") {
        let candidate = text[start..]
            .split(|c: char| c.is_whitespace() || matches!(c, ')' | ']' | '>' | '"' | '\'' | '<'))
            .next()
            .unwrap_or("");
        let Ok(url) = Url::parse(candidate.trim_end_matches(&['.', ',', ';', '!', '?'][..])) else {
            continue;
        };
        if !matches!(url.host_str(), Some("are.na" | "www.are.na")) {
            continue;
        }
        let mut segments = url.path_segments().into_iter().flatten();
        if segments.next() != Some("block") {
            continue;
        }
        if let (Some(id), None) = (segments.next(), segments.next()) {
            if let Ok(id) = id.parse::<i64>() {
                if seen.insert(id) {
                    ids.push(id);
                }
            }
        }
        if ids.len() == MAX_LINKS_PER_BLOCK {
            break;
        }
    }
    ids
}

fn linked_text_from_value(value: &Value) -> Option<LinkedText> {
    if value.get("type")?.as_str()? != "Text" {
        return None;
    }
    let body = nested_text(value, &["content", "markdown"])
        .or_else(|| nested_text(value, &["content", "plain"]))?;
    let id = value.get("id")?.as_i64()?;
    let title = value
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("Untitled");
    Some(LinkedText {
        id,
        title: title.chars().take(120).collect(),
        body: body.chars().take(MAX_LINKED_TEXT_CHARS).collect(),
    })
}

async fn hydrate_linked_text(
    client: &Client,
    base: &Url,
    page: &ContentsPage,
    token: Option<&str>,
    cache: &mut BTreeMap<i64, Option<LinkedText>>,
    fetches: &mut usize,
) -> BTreeMap<i64, Vec<LinkedText>> {
    for item in &page.data {
        if let Some(id) = item.get("id").and_then(Value::as_i64) {
            cache
                .entry(id)
                .or_insert_with(|| linked_text_from_value(item));
        }
    }
    let mut by_parent = BTreeMap::new();
    for item in &page.data {
        if item.get("type").and_then(Value::as_str) != Some("Text") {
            continue;
        }
        let Some(parent_id) = item.get("id").and_then(Value::as_i64) else {
            continue;
        };
        let Some(body) = nested_text(item, &["content", "markdown"])
            .or_else(|| nested_text(item, &["content", "plain"]))
        else {
            continue;
        };
        let mut linked = Vec::new();
        for id in linked_block_ids(body) {
            if id == parent_id {
                continue;
            }
            if !cache.contains_key(&id) && *fetches < MAX_LINK_FETCHES {
                *fetches += 1;
                let fetched =
                    get_json::<Value>(client, base, &["blocks", &id.to_string()], &[], token)
                        .await
                        .ok()
                        .and_then(|value| linked_text_from_value(&value));
                cache.insert(id, fetched);
            }
            if let Some(Some(text)) = cache.get(&id) {
                linked.push(text.clone());
            }
        }
        if !linked.is_empty() {
            by_parent.insert(parent_id, linked);
        }
    }
    by_parent
}

fn normalize_channel(
    source_name: &str,
    channel: Channel,
    page: ContentsPage,
    linked_by_parent: &BTreeMap<i64, Vec<LinkedText>>,
) -> Result<NormalizedChannel> {
    let entity = collection_key(source_name, channel.id);
    let mut documents = Vec::new();
    let mut occurrences = Vec::new();
    let mut connected_dates = Vec::new();
    let mut by_month = BTreeMap::new();
    let mut nested_channels = 0usize;
    let mut excluded_images = 0usize;

    for item in &page.data {
        let Some(connection) = item.get("connection") else {
            continue;
        };
        let connected_at = required_str(connection, "connected_at")?;
        let connected_ms = parse_time(connected_at, "connection.connected_at")?;
        connected_dates.push((connected_ms, connected_at.to_string()));
        *by_month
            .entry(connected_at.get(..7).unwrap_or("unknown").to_string())
            .or_insert(0) += 1;

        let id = required_i64(item, "id")?;
        let base_type = item
            .get("base_type")
            .and_then(Value::as_str)
            .unwrap_or("Block");
        let kind = item
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or(base_type);
        let is_channel = base_type == "Channel" || kind == "Channel";
        nested_channels += usize::from(is_channel);
        if kind == "Image" {
            excluded_images += 1;
            continue;
        }
        let source_ref = if is_channel {
            format!("arena:channel:{id}")
        } else {
            format!("arena:block:{id}")
        };
        let title = item
            .get("title")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("Are.na {kind} {id}"));
        let created_at = item
            .get("created_at")
            .and_then(Value::as_str)
            .map(|value| parse_time(value, "item.created_at"))
            .transpose()?
            .unwrap_or(connected_ms);
        let modified_at = item
            .get("updated_at")
            .and_then(Value::as_str)
            .map(|value| parse_time(value, "item.updated_at"))
            .transpose()?
            .unwrap_or(created_at);
        let public_url = if is_channel {
            let nested_identifier = item
                .get("slug")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| id.to_string());
            format!("https://www.are.na/channel/{}", nested_identifier)
        } else {
            format!("https://www.are.na/block/{id}")
        };
        let content = render_item_content(
            item,
            kind,
            &public_url,
            linked_by_parent.get(&id).map(Vec::as_slice).unwrap_or(&[]),
        );
        let mut document = structured_document_to_processed_document(StructuredDocumentInput {
            source_ref: source_ref.clone(),
            title,
            content: Some(content.clone()),
            notes: None,
            source: Some(public_url),
            tags: Vec::new(),
            links: Vec::new(),
            folders: Vec::new(),
            created_at,
        });
        document.modified_at = modified_at;
        documents.push(document);
        occurrences.push((
            entity.clone(),
            EntityOccurrence {
                id: None,
                entity_id: 0,
                file_path: source_ref,
                context: Some(representative_excerpt(&content)),
                created_at: epoch_now_ms(),
                effective_date: Some(connected_ms),
                date_source: "arena_connection".into(),
                weight: if connection.get("pinned").and_then(Value::as_bool) == Some(true) {
                    1.25
                } else {
                    1.0
                },
            },
        ));
    }

    connected_dates.sort_by_key(|(ms, _)| *ms);
    let fetched = connected_dates.len();
    let reported = page.meta.total_count.max(channel.counts.contents);
    let coverage = ChannelCoverage {
        channel_id: channel.id,
        slug: channel.slug.clone(),
        title: channel.title.clone(),
        entity,
        reported_connections: reported,
        fetched_connections: fetched,
        indexed_documents: documents.len(),
        excluded_images,
        nested_channels,
        oldest_connected_at: connected_dates.first().map(|(_, value)| value.clone()),
        newest_connected_at: connected_dates.last().map(|(_, value)| value.clone()),
        connected_by_month: by_month,
        partial: fetched < reported,
    };
    Ok(NormalizedChannel {
        documents,
        occurrences,
        coverage,
    })
}

fn render_item_content(
    item: &Value,
    kind: &str,
    public_url: &str,
    linked: &[LinkedText],
) -> String {
    let mut sections = vec![format!("Type: {kind}")];
    if let Some(value) = nested_text(item, &["description", "markdown"])
        .or_else(|| nested_text(item, &["description", "plain"]))
    {
        push_section(&mut sections, "Caption", value);
    }
    if let Some(value) = nested_text(item, &["content", "markdown"])
        .or_else(|| nested_text(item, &["content", "plain"]))
    {
        push_section(&mut sections, "Text", value);
    }
    for source in linked {
        sections.push(format!(
            "Linked Are.na text ({}):\nSource: https://www.are.na/block/{}\n{}",
            source.title, source.id, source.body
        ));
    }
    if let Some(value) = nested_text(item, &["image", "alt_text"]) {
        push_section(&mut sections, "Alt text", value);
    }
    if let Some(source) = item.get("source") {
        let title = source.get("title").and_then(Value::as_str).unwrap_or("");
        let url = source.get("url").and_then(Value::as_str).unwrap_or("");
        let value = [title, url]
            .into_iter()
            .filter(|part| !part.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        push_section(&mut sections, "Original source", &value);
    }
    push_section(&mut sections, "Are.na", public_url);
    sections.join("\n\n")
}

fn nested_text<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut current = value;
    for part in path {
        current = current.get(*part)?;
    }
    current.as_str().filter(|text| !text.trim().is_empty())
}

fn push_section(sections: &mut Vec<String>, label: &str, value: &str) {
    if !value.trim().is_empty() {
        sections.push(format!("{label}:\n{}", value.trim()));
    }
}

fn required_str<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .with_context(|| format!("Are.na response is missing string field {field}"))
}

fn required_i64(value: &Value, field: &str) -> Result<i64> {
    value
        .get(field)
        .and_then(Value::as_i64)
        .with_context(|| format!("Are.na response is missing integer field {field}"))
}

fn parse_time(value: &str, field: &str) -> Result<i64> {
    DateTime::parse_from_rfc3339(value)
        .map(|date| date.timestamp_millis())
        .with_context(|| format!("invalid {field} timestamp"))
}

fn build_program(
    workspace_name: &str,
    channels: &[NormalizedChannel],
    sampling: SamplingMode,
    budget: usize,
) -> Program {
    let learning = sampling.learning(budget);
    let mut title_counts = BTreeMap::<String, usize>::new();
    let mut reserved_selectors = BTreeMap::<String, i64>::new();
    for item in channels {
        reserved_selectors.insert(
            item.coverage.channel_id.to_string(),
            item.coverage.channel_id,
        );
        reserved_selectors.insert(
            item.coverage.entity.to_lowercase(),
            item.coverage.channel_id,
        );
        *title_counts
            .entry(item.coverage.title.to_lowercase())
            .or_default() += 1;
    }
    let readings = channels
        .iter()
        .map(|item| Reading {
            entity: format!("channel:{}", item.coverage.channel_id),
            profile: "auto".into(),
            definition: None,
            learning: learning.clone(),
            include_linked_pages: false,
            include_who_links: false,
            pattern: false,
        })
        .collect();
    Program {
        workspaces: vec![Workspace {
            name: workspace_name.to_string(),
            sources: vec![Source::Arena(ArenaSource {
                name: ARENA_SOURCE_NAME.to_string(),
                channels: channels
                    .iter()
                    .map(|item| ArenaChannel {
                        remote_id: item.coverage.channel_id.to_string(),
                        alias: (title_counts[&item.coverage.title.to_lowercase()] == 1
                            && reserved_selectors
                                .get(&item.coverage.title.to_lowercase())
                                .is_none_or(|owner| *owner == item.coverage.channel_id))
                        .then(|| item.coverage.title.clone()),
                        slug: None,
                        title: None,
                    })
                    .collect(),
            })],
            learning,
            readings,
            format: None,
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn render_program_with_receipt(
    program: &Program,
    channels: &[ChannelCoverage],
    sampling: SamplingMode,
) -> String {
    let mut header =
        String::from("// Are.na bounded channel import. Credentials are never stored here.\n");
    header.push_str(&format!(
        "// Catalyst evidence: {}. API fetch coverage is recorded separately.\n",
        sampling.canonical()
    ));
    for channel in channels {
        let title = channel.title.replace(['\r', '\n'], " ");
        header.push_str(&format!(
            "// {} => collection {} ({} of {} connections in fetched window; {})\n",
            title,
            channel.entity,
            channel.fetched_connections,
            channel.reported_connections,
            if channel.partial {
                "partial"
            } else {
                "complete"
            }
        ));
    }
    let rendered = enzyme_spec::render_program(program);
    format!("{header}\n{rendered}")
}

fn declared_channel_scope(
    source: &str,
    workspace_name: &str,
    program_path: &Path,
) -> Result<DeclaredChannelScope> {
    let program = enzyme_spec::parse(source)
        .with_context(|| format!("parse existing {}", program_path.display()))?;
    let workspace = program
        .workspaces
        .iter()
        .find(|workspace| workspace.name == workspace_name)
        .with_context(|| {
            format!(
                "existing {} has no workspace {:?}",
                program_path.display(),
                workspace_name
            )
        })?;
    let arena = workspace
        .sources
        .iter()
        .find_map(|source| match source {
            Source::Arena(source) if source.name == ARENA_SOURCE_NAME => Some(source),
            _ => None,
        })
        .with_context(|| {
            format!(
                "existing {} has no source arena {:?}",
                program_path.display(),
                ARENA_SOURCE_NAME
            )
        })?;
    let mut selectors = BTreeMap::new();
    let mut remote_ids = Vec::new();
    for channel in &arena.channels {
        selectors.insert(channel.remote_id.to_lowercase(), channel.remote_id.clone());
        if let Some(alias) = &channel.alias {
            selectors.insert(alias.to_lowercase(), channel.remote_id.clone());
        }
        if let Some(slug) = &channel.slug {
            selectors.insert(slug.to_lowercase(), channel.remote_id.clone());
        }
        remote_ids.push(channel.remote_id.clone());
    }
    Ok(DeclaredChannelScope {
        selectors,
        remote_ids,
    })
}

fn validate_existing_program(
    source: &str,
    workspace_name: &str,
    channels: &[ChannelCoverage],
    program_path: &Path,
) -> Result<String> {
    let program = enzyme_spec::parse(source)
        .with_context(|| format!("parse existing {}", program_path.display()))?;
    let workspace = program
        .workspaces
        .iter()
        .find(|workspace| workspace.name == workspace_name)
        .with_context(|| {
            format!(
                "existing {} has no workspace {:?}; edit or remove it before importing",
                program_path.display(),
                workspace_name
            )
        })?;
    let declared = workspace
        .sources
        .iter()
        .find_map(|source| match source {
            Source::Arena(source) if source.name == ARENA_SOURCE_NAME => Some(source),
            _ => None,
        })
        .with_context(|| {
            format!(
                "existing {} has no source arena {:?}",
                program_path.display(),
                ARENA_SOURCE_NAME
            )
        })?;
    for channel in channels {
        ensure!(
            declared
                .channels
                .iter()
                .any(|candidate| { candidate.remote_id == channel.channel_id.to_string() }),
            "channel {} ({}) is outside the source scope in {}; edit the source declaration before importing it",
            channel.slug,
            channel.channel_id,
            program_path.display()
        );
    }
    let resolved = enzyme_spec::resolve_in(
        vec![program],
        &enzyme_core::config::default_spec_environment()?,
    )?;
    let vault = resolved
        .vaults
        .iter()
        .find(|vault| vault.path == format!("workspace:{workspace_name}"))
        .context("resolved workspace reading is missing")?;
    let mut policies = BTreeSet::new();
    for channel in channels {
        let entity = format!("collection:{}", channel.entity);
        let reading = vault
            .readings
            .iter()
            .find(|reading| reading.entity == entity);
        let policy = reading.map_or_else(
            || "not sampled (no reading)".to_string(),
            |reading| {
                reading
                    .learning
                    .sampling_mode()
                    .or_else(|| vault.learning.sampling_mode())
                    .map(|mode| format!("sample by {mode}"))
                    .unwrap_or_else(|| "default sampling".to_string())
            },
        );
        policies.insert(policy);
    }
    Ok(if policies.len() == 1 {
        policies.into_iter().next().unwrap()
    } else {
        "per-channel policies in preserved program".to_string()
    })
}

fn arena_program_path(paths: &EnzymePaths, workspace_name: &str) -> PathBuf {
    let safe_name: String = workspace_name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '-'
            }
        })
        .collect();
    paths
        .home
        .join("configs")
        .join(format!("arena-{safe_name}.enzyme"))
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "_+-./:@".contains(ch))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

fn print_coverage(channels: &[ChannelCoverage], policy: &str, plan_command: &str) {
    println!("Catalyst evidence: {policy}");
    println!("Planned excerpt and question allocation by period: {plan_command}");
    println!("This read-only plan is pre-generation evidence, not a completed epoch trace.");
    println!("API fetch: created_at_desc; date buckets describe the fetched window only.");
    for channel in channels {
        println!(
            "- {}: fetched {} of {} connections ({}); {} indexed, {} images excluded",
            channel.title,
            channel.fetched_connections,
            channel.reported_connections,
            if channel.partial {
                "partial"
            } else {
                "complete"
            },
            channel.indexed_documents,
            channel.excluded_images,
        );
        if let (Some(oldest), Some(newest)) = (
            channel.oldest_connected_at.as_deref(),
            channel.newest_connected_at.as_deref(),
        ) {
            println!("  connected_at: {oldest} .. {newest}");
        }
        if !channel.connected_by_month.is_empty() {
            let buckets = channel
                .connected_by_month
                .iter()
                .map(|(month, count)| format!("{month}={count}"))
                .collect::<Vec<_>>()
                .join(", ");
            println!("  fetched-window distribution: {buckets}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    #[tokio::test]
    async fn explicit_linked_text_is_parent_evidence_without_new_occurrence() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v3/blocks/77"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 77, "type": "Text", "title": "Earlier note",
                "content": {"markdown": "The linked source makes the reference understandable."}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let page: ContentsPage = serde_json::from_value(serde_json::json!({
            "meta": {"total_count": 1},
            "data": [{
                "id": 42, "base_type": "Block", "type": "Text", "title": "Response",
                "content": {"markdown": "This responds to https://www.are.na/block/77 and https://www.are.na/block/77"},
                "connection": {"connected_at": "2025-02-03T04:05:06Z"}
            }]
        })).unwrap();
        let base = normalized_api_base(&format!("{}/v3/", server.uri())).unwrap();
        let mut cache = BTreeMap::new();
        let mut fetches = 0;
        let linked =
            hydrate_linked_text(&Client::new(), &base, &page, None, &mut cache, &mut fetches).await;
        let channel: Channel = serde_json::from_value(serde_json::json!({
            "id": 9, "slug": "research", "title": "Research", "counts": {"contents": 1}
        }))
        .unwrap();
        let result = normalize_channel(ARENA_SOURCE_NAME, channel, page, &linked).unwrap();
        assert_eq!(fetches, 1);
        assert_eq!(result.documents.len(), 1);
        assert_eq!(result.occurrences.len(), 1);
        let content = &result.documents[0].content;
        assert!(content.contains("The linked source makes the reference understandable."));
        assert!(content.contains("https://www.are.na/block/77"));
        assert_eq!(
            linked_block_ids("https://evilare.na/block/1 https://www.are.na/channel/2"),
            Vec::<i64>::new()
        );
    }

    #[test]
    fn channel_url_parses_without_leaking_owner_into_identity() {
        assert_eq!(
            channel_identifier("https://www.are.na/are-na-team/arena-influences/").unwrap(),
            "arena-influences"
        );
        assert!(channel_identifier("https://example.com/person/channel").is_err());
        assert_eq!(shell_quote("/tmp/arena vault"), "'/tmp/arena vault'");
        assert_eq!(shell_quote("a'b"), "'a'\"'\"'b'");
    }

    #[test]
    fn normalization_keeps_block_identity_and_connection_time() {
        let channel: Channel = serde_json::from_value(serde_json::json!({
            "id": 9,
            "slug": "research",
            "title": "Research",
            "description": null,
            "created_at": "2024-01-01T00:00:00Z",
            "updated_at": "2024-02-01T00:00:00Z",
            "counts": {"contents": 8}
        }))
        .unwrap();
        let page: ContentsPage = serde_json::from_value(serde_json::json!({
            "meta": {"total_count": 8},
            "data": [{
                "id": 42,
                "type": "Text",
                "base_type": "Block",
                "title": "A block",
                "created_at": "2020-01-01T00:00:00Z",
                "updated_at": "2024-01-02T00:00:00Z",
                "content": {"markdown": "Useful text"},
                "description": null,
                "source": null,
                "connection": {
                    "id": 77,
                    "connected_at": "2024-03-04T05:06:07Z",
                    "pinned": false
                }
            }]
        }))
        .unwrap();
        let normalized =
            normalize_channel(ARENA_SOURCE_NAME, channel, page, &BTreeMap::new()).unwrap();
        assert_eq!(normalized.documents[0].source_ref, "arena:block:42");
        assert_eq!(normalized.occurrences[0].0, "arena:channels/channel/9");
        assert_eq!(
            normalized.occurrences[0].1.effective_date,
            Some(parse_time("2024-03-04T05:06:07Z", "test").unwrap())
        );
        assert!(normalized.coverage.partial);
        assert_eq!(normalized.coverage.connected_by_month["2024-03"], 1);
    }

    #[test]
    fn generated_program_uses_current_sampling_language() {
        let normalized = NormalizedChannel {
            coverage: ChannelCoverage {
                channel_id: 9,
                slug: "research".into(),
                title: "Research".into(),
                entity: "arena:channels/channel/9".into(),
                reported_connections: 0,
                fetched_connections: 0,
                indexed_documents: 0,
                excluded_images: 0,
                nested_channels: 0,
                oldest_connected_at: None,
                newest_connected_at: None,
                connected_by_month: BTreeMap::new(),
                partial: false,
            },
            documents: Vec::new(),
            occurrences: Vec::new(),
        };
        let program = build_program("arena-test", &[normalized], SamplingMode::Recency, 15);
        let rendered = enzyme_spec::render_program(&program);
        assert!(rendered.contains("sample by recency"), "{rendered}");
        assert!(rendered.contains("workspace \"arena-test\""), "{rendered}");
        assert!(rendered.contains("source arena \"channels\""), "{rendered}");
        assert!(rendered.contains("channel 9 as \"Research\""), "{rendered}");
        assert!(
            rendered.contains("learn questions from channel 9"),
            "{rendered}"
        );
        let parsed = enzyme_spec::parse(&rendered).unwrap();
        let resolved = enzyme_spec::resolve(vec![parsed], Path::new("/")).unwrap();
        assert_eq!(resolved.vaults[0].path, "workspace:arena-test");
        assert_eq!(
            resolved.vaults[0].readings[0].entity,
            "collection:arena:channels/channel/9"
        );
        assert_eq!(
            validate_existing_program(
                &rendered,
                "arena-test",
                &[ChannelCoverage {
                    channel_id: 9,
                    slug: "research".into(),
                    title: "Research".into(),
                    entity: "arena:channels/channel/9".into(),
                    reported_connections: 0,
                    fetched_connections: 0,
                    indexed_documents: 0,
                    excluded_images: 0,
                    nested_channels: 0,
                    oldest_connected_at: None,
                    newest_connected_at: None,
                    connected_by_month: BTreeMap::new(),
                    partial: false,
                }],
                Path::new("/tmp/config.enzyme"),
            )
            .unwrap(),
            "sample by recency"
        );
        // An existing program may also declare a built-in source kind.
        let with_notes = rendered.replacen(
            "  source arena \"channels\"",
            "  source apple-notes \"notes\" {}\n  source arena \"channels\"",
            1,
        );
        assert!(with_notes.contains("source apple-notes"), "{with_notes}");
        let coverage = ChannelCoverage {
            channel_id: 9,
            slug: "research".into(),
            title: "Research".into(),
            entity: "arena:channels/channel/9".into(),
            reported_connections: 0,
            fetched_connections: 0,
            indexed_documents: 0,
            excluded_images: 0,
            nested_channels: 0,
            oldest_connected_at: None,
            newest_connected_at: None,
            connected_by_month: BTreeMap::new(),
            partial: false,
        };
        validate_existing_program(
            &with_notes,
            "arena-test",
            &[coverage],
            Path::new("/tmp/config.enzyme"),
        )
        .unwrap();
        let scope =
            declared_channel_scope(&rendered, "arena-test", Path::new("/tmp/config.enzyme"))
                .unwrap();
        assert_eq!(
            scope.selectors.get("research").map(String::as_str),
            Some("9")
        );
        assert_eq!(scope.remote_ids, ["9"]);
    }

    #[test]
    fn generated_program_omits_ambiguous_title_aliases() {
        let make_channel = |id| NormalizedChannel {
            coverage: ChannelCoverage {
                channel_id: id,
                slug: format!("research-{id}"),
                title: "Research".into(),
                entity: collection_key(ARENA_SOURCE_NAME, id),
                reported_connections: 0,
                fetched_connections: 0,
                indexed_documents: 0,
                excluded_images: 0,
                nested_channels: 0,
                oldest_connected_at: None,
                newest_connected_at: None,
                connected_by_month: BTreeMap::new(),
                partial: false,
            },
            documents: Vec::new(),
            occurrences: Vec::new(),
        };
        let program = build_program(
            "arena-test",
            &[make_channel(9), make_channel(10)],
            SamplingMode::Recency,
            15,
        );
        let rendered = enzyme_spec::render_program(&program);
        assert!(rendered.contains("channel 9\n"), "{rendered}");
        assert!(rendered.contains("channel 10\n"), "{rendered}");
        assert!(!rendered.contains(" as \"Research\""), "{rendered}");
        let parsed = enzyme_spec::parse(&rendered).unwrap();
        enzyme_spec::resolve(vec![parsed], Path::new("/")).unwrap();
    }
}
