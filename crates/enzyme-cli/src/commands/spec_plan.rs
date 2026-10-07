//! `enzyme spec plan`: what a reading program would do on an indexed vault.
//!
//! Strictly read-only. The index is opened with SQLite read-only flags and no
//! migrations; selection and job construction are the same functions init
//! runs, with no LLM client call, no embedding computation and no writes.
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use enzyme_core::config::{EnzymeConfig, EnzymePaths};
use enzyme_core::db::Database;
use enzyme_core::llm::{DEFAULT_FORMAT_KEY, DEFAULT_PROFILE_KEY, LlmClient};
use enzyme_core::pipeline::catalysts::{
    CatalystConfig, CatalystPipeline, JobsPlan, PlannedJob, PlannedSkip, SkipReason,
    calculate_allocations,
};
use enzyme_core::pipeline::context::ContextBuilder;
use enzyme_core::pipeline::selection::{SelectionPipeline, SelectionResult};
use enzyme_core::pipeline::selection_steps::{
    EXPANDABLE_DENSITY_THRESHOLD, FolderExpansion, MAX_FOLDER_CHILDREN, PatternDiagnostics,
};
use enzyme_spec::{AsOf, Learning, Program, Reading, SelectionAmount, SelectionMode};

use super::init::EvidenceLimits;

pub struct PlanOptions<'a> {
    /// Optional with `workspace`: the workspace's own program file.
    pub file: Option<&'a Path>,
    pub vault: Option<&'a Path>,
    /// A named workspace: plan its declaration against its home-store index.
    pub workspace: Option<&'a str>,
    pub json: bool,
    /// Most entities and skips listed in `entities`/`skipped`.
    pub limit: Option<usize>,
    pub prompts: Option<&'a Path>,
    pub local: bool,
}

/// A setting's effective value and the scope that supplied it.
#[derive(Debug, Serialize)]
pub struct Sourced {
    pub value: String,
    /// `reading`, `vault`, `global`, or `default`.
    pub from: &'static str,
}

#[derive(Debug, Serialize)]
pub struct LearningPlan {
    pub sampling: Sourced,
    pub periods: Sourced,
    pub budget: Sourced,
    pub select: Sourced,
    pub as_of: Sourced,
}

#[derive(Debug, Serialize)]
pub struct ReadingPlan {
    /// The source as written, e.g. `folder "people" including linked pages`.
    pub source: String,
    pub kind: String,
    pub name: String,
    pub pattern: bool,
    pub including_linked_pages: bool,
    pub including_who_links: bool,
    pub profile: String,
    pub custom_profile: bool,
    pub learning: LearningPlan,
    /// Folder page-link expansion, for folder readings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expansion: Option<FolderExpansion>,
    /// Name-pattern resolution, for `tags|links matching`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matches: Option<PatternDiagnostics>,
    /// Source-scoped `who` link selection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub who_links: Option<PatternDiagnostics>,
    pub jobs: Vec<PlannedJob>,
    pub skipped: Vec<PlannedSkip>,
    /// Why this reading produced fewer jobs than it names, in plain words.
    pub notes: Vec<String>,
    /// The same reasons as codes, for the top-level `skipped` list.
    #[serde(skip)]
    pub skips: Vec<Skip>,
}

/// One selected entity: where it came from and whether it gets a job.
#[derive(Debug, Serialize)]
pub struct EntityPlan {
    /// `folder`, `tag`, `link`, `log`, or `collection`.
    pub kind: String,
    pub name: String,
    /// `reading`, `pattern`, `who_link`, `expanded`, or `automatic`.
    pub origin: &'static str,
    /// Index into `readings` of the reading that selected it, if any.
    pub reading: Option<usize>,
    /// For `expanded`: the folder whose linked page this is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expanded_from: Option<String>,
    /// `planned` (gets a generation job) or `skipped` (see `skip`).
    pub status: &'static str,
    /// Questions its job asks for; 0 when skipped.
    pub questions: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip: Option<SkipCode>,
}

/// A machine reason code and a short human reason.
#[derive(Debug, Clone, Serialize)]
pub struct SkipCode {
    pub code: &'static str,
    pub reason: String,
}

/// A reading, candidate group, or selected entity that yields no job.
#[derive(Debug, Clone, Serialize)]
pub struct Skip {
    /// `entity` (selected, but no job), `reading` (the declared entity
    /// itself), `candidates` (`count` unselected matches or linked pages),
    /// or `automatic`.
    pub scope: &'static str,
    #[serde(flatten)]
    pub code: SkipCode,
    pub reading: Option<usize>,
    /// The entity, for `entity` and `reading` scopes.
    pub kind: Option<String>,
    pub name: Option<String>,
    pub count: usize,
}

#[derive(Debug, Default, Serialize)]
pub struct Counts {
    /// Selected entities (the length of the untruncated `entities`).
    pub selected: usize,
    /// Selected entities with a generation job.
    pub planned: usize,
    /// Selected entities without one.
    pub skipped: usize,
    /// Selected entities per `origin`.
    pub by_origin: std::collections::BTreeMap<&'static str, usize>,
    /// Unselected candidates and readings, summed over non-`entity` skips.
    pub candidates_skipped: usize,
}

/// How many entries `--limit` left out of each kind of list.
#[derive(Debug, Default, Serialize)]
pub struct Truncated {
    pub limit: usize,
    pub entities: usize,
    pub skipped: usize,
    /// Summed over `readings[].jobs` and `other_jobs`.
    pub jobs: usize,
    /// Summed over `readings[].skipped` and `other_skipped`.
    pub job_skips: usize,
}

fn cut<T>(list: &mut Vec<T>, n: usize) -> usize {
    let omitted = list.len().saturating_sub(n);
    list.truncate(n);
    omitted
}

impl VaultPlan {
    /// Bound every entity list to `n` entries; `counts` and `totals` keep
    /// describing the whole plan.
    fn truncate(&mut self, n: usize) {
        let mut t = Truncated {
            limit: n,
            entities: cut(&mut self.entities, n),
            skipped: cut(&mut self.skipped, n),
            jobs: cut(&mut self.other_jobs, n),
            job_skips: cut(&mut self.other_skipped, n),
        };
        for r in &mut self.readings {
            t.jobs += cut(&mut r.jobs, n);
            t.job_skips += cut(&mut r.skipped, n);
        }
        if t.entities + t.skipped + t.jobs + t.job_skips > 0 {
            self.truncated = Some(t);
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Reference {
    pub instant: String,
    pub ms: i64,
    /// `wall clock`, `date`, or `latest evidence`.
    pub source: &'static str,
}

#[derive(Debug, Serialize)]
pub struct Totals {
    pub jobs: usize,
    pub requested_questions: usize,
    pub excerpts: usize,
    pub skipped: usize,
}

#[derive(Debug, Serialize)]
pub struct VaultPlan {
    /// The named workspace planned with `--workspace`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    pub vault: String,
    /// The path the program declares, when planned against another vault.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub declared_vault: Option<String>,
    pub database: String,
    pub reference: Reference,
    /// `hosted` or `local` prompt construction.
    pub prompts: &'static str,
    /// `readings`, `automatic` coverage selection when none are declared, or
    /// `readings and automatic` (`learn questions automatically`).
    pub selection: &'static str,
    pub readings: Vec<ReadingPlan>,
    /// How `learn questions automatically` split the automatic limit, and
    /// what it picked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub automatic: Option<enzyme_core::pipeline::selection_steps::AutomaticDiagnostics>,
    /// Jobs no reading accounts for: automatic selection's.
    pub other_jobs: Vec<PlannedJob>,
    pub other_skipped: Vec<PlannedSkip>,
    pub totals: Totals,
    /// Every selected entity with its origin and status.
    pub entities: Vec<EntityPlan>,
    /// Everything left out, with reason codes; see docs/cli-json.md.
    pub skipped: Vec<Skip>,
    pub counts: Counts,
    /// Present only when `--limit` shortened `entities` or `skipped`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<Truncated>,
}

/// Names the `spec plan --json` shape; see docs/cli-json.md.
pub const SPEC_PLAN_SCHEMA: &str = "enzyme.spec-plan.v1";

pub fn run(options: PlanOptions<'_>) -> Result<()> {
    let plans = plan(&options)?;
    if options.json {
        let vaults: Vec<&VaultPlan> = plans.iter().map(|(plan, _)| plan).collect();
        println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({ "schema": SPEC_PLAN_SCHEMA, "vaults": vaults })
            )?
        );
    } else {
        for (i, (plan, _)) in plans.iter().enumerate() {
            if i > 0 {
                println!();
            }
            print!("{}", render(plan));
        }
    }
    if let Some(dir) = options.prompts {
        let written = write_prompts(dir, &plans)?;
        eprintln!("Wrote {written} prompt files to {}", dir.display());
    }
    Ok(())
}

/// The program file that declares `workspace` in `$ENZYME_HOME/configs`.
fn workspace_program_file(workspace: &str) -> Result<PathBuf> {
    let configs = EnzymePaths::enzyme_home()?.join("configs");
    let user_home = dirs::home_dir().context("cannot resolve home directory")?;
    let (target, _) = enzyme_spec::plan::ConfigStore::new(&configs, user_home)
        .locate(workspace)
        .map_err(super::exit::CliFailure::config)?;
    let file = configs.join(target);
    if !file.is_file() {
        return Err(super::exit::CliFailure::config(anyhow::anyhow!(
            "no program in {} declares workspace {workspace:?}",
            configs.display()
        )));
    }
    Ok(file)
}

fn declares_workspace(program: &Program, workspace: &str) -> bool {
    program.workspaces.iter().any(|w| w.name == workspace)
}

/// Resolve the file like the runtime would and plan each selected vault.
fn plan(options: &PlanOptions<'_>) -> Result<Vec<(VaultPlan, JobsPlan)>> {
    let workspace = options.workspace;
    let workspace_root = workspace
        .map(EnzymePaths::workspace_root)
        .transpose()
        .map_err(super::exit::CliFailure::usage)?;
    let located;
    let file = match (options.file, workspace) {
        (Some(file), _) => file,
        (None, Some(name)) => {
            located = workspace_program_file(name)?;
            located.as_path()
        }
        (None, None) => {
            return Err(super::exit::CliFailure::usage(anyhow::anyhow!(
                "spec plan needs a program file or --workspace <name>"
            )));
        }
    };
    let source =
        std::fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
    let ast = enzyme_spec::parse(&source)
        .with_context(|| format!("parse {}", file.display()))
        .map_err(super::exit::CliFailure::config)?;
    if let Some(name) = workspace
        && !declares_workspace(&ast, name)
    {
        return Err(super::exit::CliFailure::config(anyhow::anyhow!(
            "{} does not declare workspace {name:?}",
            file.display()
        )));
    }
    let user_home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME missing")?;
    // A file inside the runtime configs directory shares its namespace (e.g.
    // profiles defined in profiles.enzyme), so resolve it with its siblings.
    // Planning a named workspace always resolves against the namespace; the
    // candidate replaces the file that declares the workspace today.
    // Another workspace's invalid file does not block planning this one.
    let mut others = Vec::new();
    let directory = match workspace {
        Some(_) => Some(EnzymePaths::enzyme_home()?.join("configs")).filter(|d| d.is_dir()),
        None => runtime_configs_directory(file)?,
    };
    if let Some(directory) = directory {
        for entry in std::fs::read_dir(&directory)? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "enzyme")
                && path.is_file()
                && !same_file(&path, file)
                && let Ok(program) = enzyme_spec::parse(&std::fs::read_to_string(&path)?)
                && !workspace.is_some_and(|name| declares_workspace(&program, name))
            {
                others.push(program);
            }
        }
    }
    let globals: Vec<Learning> = std::iter::once(&ast)
        .chain(&others)
        .map(|p| p.learning.clone())
        .collect();
    let raw_scopes: Vec<enzyme_spec::Vault> = ast
        .vaults
        .iter()
        .cloned()
        .chain(
            ast.workspaces
                .iter()
                .map(|workspace| workspace.unresolved_vault()),
        )
        .collect();
    let environment =
        enzyme_core::config::spec_environment(&user_home, &EnzymePaths::enzyme_home()?)?;
    let mut resolved = enzyme_spec::resolve_with_in(others, ast.clone(), &environment)?.program;
    // The candidate resolves first and fully, so its vaults/workspaces come first.
    resolved.vaults.truncate(raw_scopes.len());
    anyhow::ensure!(
        !resolved.vaults.is_empty(),
        "{} declares no vault",
        file.display()
    );

    let mut targets = Vec::new();
    match (workspace, options.vault) {
        (Some(name), _) => {
            let index = resolved
                .vaults
                .iter()
                .position(|v| v.workspace.as_deref() == Some(name))
                .with_context(|| format!("{} does not declare workspace {name:?}", file.display()))
                .map_err(super::exit::CliFailure::config)?;
            let root = workspace_root.clone().expect("workspace root resolved");
            targets.push((index, root, None));
        }
        (None, Some(vault)) => {
            let canonical = vault
                .canonicalize()
                .with_context(|| format!("vault {} not found", vault.display()))?;
            let key = canonical.to_string_lossy().to_string();
            if let Some(i) = resolved.vaults.iter().position(|v| v.path == key) {
                targets.push((i, canonical, None));
            } else if resolved.vaults.len() == 1 {
                let declared = resolved.vaults[0].path.clone();
                targets.push((0, canonical, Some(declared)));
            } else {
                bail!(
                    "{} declares several vaults and none is {}; declared: {}",
                    file.display(),
                    key,
                    resolved
                        .vaults
                        .iter()
                        .map(|v| v.path.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
        (None, None) => {
            for (i, v) in resolved.vaults.iter().enumerate() {
                targets.push((i, PathBuf::from(&v.path), None));
            }
        }
    }

    let mut plans = Vec::new();
    for (index, vault_path, declared) in targets {
        let mut program = resolved.clone();
        let mut vault = program.vaults.swap_remove(index);
        // A named workspace keeps its declared sources; its index is the
        // home store, which `EnzymeConfig` finds from `vault_path`.
        if workspace.is_none() && !vault.path.starts_with("workspace:") {
            vault.path = vault_path.to_string_lossy().to_string();
        }
        program.vaults = vec![vault];
        let raw = &raw_scopes[index];
        plans.push(plan_vault(
            &vault_path,
            declared,
            workspace,
            program,
            raw,
            &globals,
            options.local,
            options.limit,
        )?);
    }
    Ok(plans)
}

fn runtime_configs_directory(file: &Path) -> Result<Option<PathBuf>> {
    let Some(parent) = file.canonicalize()?.parent().map(Path::to_path_buf) else {
        return Ok(None);
    };
    let configs = EnzymePaths::enzyme_home()?.join("configs");
    Ok(configs
        .canonicalize()
        .ok()
        .filter(|c| *c == parent)
        .map(|_| parent))
}

fn same_file(a: &Path, b: &Path) -> bool {
    matches!((a.canonicalize(), b.canonicalize()), (Ok(a), Ok(b)) if a == b)
}

#[allow(clippy::too_many_arguments)]
fn plan_vault(
    vault_path: &Path,
    declared_vault: Option<String>,
    workspace: Option<&str>,
    program: Program,
    raw: &enzyme_spec::Vault,
    globals: &[Learning],
    local: bool,
    limit: Option<usize>,
) -> Result<(VaultPlan, JobsPlan)> {
    let paths = EnzymePaths::new(vault_path)?;
    if !paths.vault_db.is_file() {
        let target = match workspace {
            Some(name) => format!("--workspace {name}"),
            None => format!("-p {}", vault_path.display()),
        };
        return Err(super::exit::CliFailure::config(anyhow::anyhow!(
            "{} is not indexed (no {}); run `enzyme {target} init --stage index` first",
            workspace.map_or_else(
                || vault_path.display().to_string(),
                |n| format!("workspace {n:?}")
            ),
            paths.vault_db.display(),
        )));
    }
    let db = Database::open_read_only(&paths.vault_db).with_context(|| {
        format!(
            "open {} read-only (an index from an older enzyme may need `enzyme refresh`)",
            paths.vault_db.display()
        )
    })?;
    let config = EnzymeConfig::from_readable_program(vault_path, program)?;
    let spec = config
        .reading_spec
        .clone()
        .context("resolved program has no reading spec for this vault")?;

    // The same selection and job construction init runs.
    let selection = SelectionPipeline::new(&db, vault_path).run_with_config(Some(&config))?;
    let vault_context = ContextBuilder::new(&db)
        .build(&selection.entities, 5, 300)
        .ok();
    let allocations = calculate_allocations(
        &selection.entities,
        config.trending.as_ref().and_then(|t| t.min_top_catalysts),
    );
    let limits = EvidenceLimits::from_env();
    let cat_config = CatalystConfig {
        reading_spec: Some(&spec),
        entity_profiles: &selection.entity_profiles,
        catalysts_per_entity: 7,
        default_profile: DEFAULT_PROFILE_KEY,
        format_key: config
            .catalyst_format
            .as_deref()
            .unwrap_or(DEFAULT_FORMAT_KEY),
        prompt_context: vault_context.as_deref(),
        model: "plan",
        per_entity_allocations: Some(&allocations),
        max_context_chars: limits.max_context_chars,
        max_samples: limits.max_samples,
        concurrency: None,
        force_regenerate: false,
        min_context_documents: limits.min_context_documents,
        min_context_tokens: limits.min_context_tokens,
        epoch_id: None,
    };
    // Never contacted: prompt construction only asks whether it is local.
    let llm = if local {
        LlmClient::with_base_url("plan", "http://localhost:0")
    } else {
        LlmClient::with_base_url("plan", "https://plan.invalid/v1")
    };
    let jobs =
        CatalystPipeline::new(&db, &llm, None).plan_jobs(&selection.entities, &cat_config)?;

    let readings = attribute(&spec, raw, globals, &selection, &jobs);
    let reference_date = match &spec.learning.as_of {
        None => None,
        Some(_) => Some(rfc3339(jobs.reference_ms)[..10].to_string()),
    };
    let (entities, skipped) = explain(&readings, &selection, &jobs, reference_date.as_deref());
    let counts = count(&entities, &skipped);
    let mut claimed = HashSet::new();
    for r in &readings {
        claimed.extend(r.jobs.iter().map(|j| key(&j.entity_type, &j.entity_name)));
        claimed.extend(
            r.skipped
                .iter()
                .map(|j| key(&j.entity_type, &j.entity_name)),
        );
    }
    let totals = Totals {
        jobs: jobs.jobs.len(),
        requested_questions: jobs.jobs.iter().map(|j| j.shape.requested_questions).sum(),
        excerpts: jobs.jobs.iter().map(|j| j.shape.excerpts).sum(),
        skipped: jobs.skipped.len(),
    };
    let plan = VaultPlan {
        workspace: workspace.map(str::to_string),
        vault: vault_path.to_string_lossy().to_string(),
        declared_vault,
        database: paths.vault_db.to_string_lossy().to_string(),
        reference: Reference {
            instant: rfc3339(jobs.reference_ms),
            ms: jobs.reference_ms,
            source: match &spec.learning.as_of {
                None => "wall clock",
                Some(AsOf::Date { .. }) => "date",
                Some(AsOf::LatestEvidence) => "latest evidence",
            },
        },
        prompts: if llm.is_local() { "local" } else { "hosted" },
        selection: match (&selection.diagnostics.automatic, spec.readings.is_empty()) {
            (Some(_), false) => "readings and automatic",
            (Some(_), true) => "automatic",
            (None, _) if config.has_curated_entities() => "readings",
            (None, _) => "automatic",
        },
        readings,
        automatic: selection.diagnostics.automatic.clone(),
        other_jobs: jobs
            .jobs
            .iter()
            .filter(|j| !claimed.contains(&key(&j.entity_type, &j.entity_name)))
            .cloned()
            .collect(),
        other_skipped: jobs
            .skipped
            .iter()
            .filter(|j| !claimed.contains(&key(&j.entity_type, &j.entity_name)))
            .cloned()
            .collect(),
        totals,
        entities,
        skipped,
        counts,
        truncated: None,
    };
    let mut plan = plan;
    if let Some(n) = limit {
        plan.truncate(n);
    }
    Ok((plan, jobs))
}

fn key(entity_type: &str, name: &str) -> (String, String) {
    (entity_type.to_string(), name.to_lowercase())
}

fn rfc3339(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map_or_else(
        || ms.to_string(),
        |d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    )
}

/// Assign built jobs and skips to the readings that produced them, and
/// explain readings that produced none.
fn attribute(
    spec: &enzyme_spec::Vault,
    raw: &enzyme_spec::Vault,
    globals: &[Learning],
    selection: &SelectionResult,
    jobs: &JobsPlan,
) -> Vec<ReadingPlan> {
    let diagnostics = &selection.diagnostics;
    let mut patterns = diagnostics.patterns.iter();
    let as_of_declared = spec.learning.as_of.is_some();
    let date = as_of_declared.then(|| rfc3339(jobs.reference_ms)[..10].to_string());
    let date = date.as_deref();
    spec.readings
        .iter()
        .enumerate()
        .map(|(i, reading)| {
            let (kind, name) = enzyme_spec::split_entity(&reading.entity);
            let entity_type = if kind == "log" { "append_log" } else { kind };
            let mut plan = ReadingPlan {
                source: source_text(reading),
                kind: kind.to_string(),
                name: name.to_string(),
                pattern: reading.pattern,
                including_linked_pages: reading.include_linked_pages,
                including_who_links: reading.include_who_links,
                profile: reading.profile.clone(),
                custom_profile: spec.profiles.contains_key(&reading.profile)
                    || reading.profile.starts_with("inline:"),
                learning: learning_plan(
                    reading,
                    raw.readings.get(i),
                    raw,
                    globals,
                    spec.learning.budget,
                ),
                expansion: None,
                matches: None,
                who_links: None,
                jobs: Vec::new(),
                skipped: Vec::new(),
                notes: Vec::new(),
                skips: Vec::new(),
            };
            let mut members: Vec<(String, String)> = Vec::new();
            if reading.pattern {
                let found = patterns.next().cloned();
                if let Some(d) = &found {
                    members.extend(d.selected.iter().map(|e| key(&e.entity_type, &e.name)));
                    pattern_notes(&mut plan.notes, d, kind, as_of_declared);
                    pattern_skips(&mut plan.skips, d, kind, name, i, date);
                }
                plan.matches = found;
            } else {
                let selector = enzyme_spec::entity_selector(kind, name);
                let declared = |code, reason: &str| Skip {
                    scope: "reading",
                    code: SkipCode {
                        code,
                        reason: reason.to_string(),
                    },
                    reading: Some(i),
                    kind: Some(kind.to_string()),
                    name: Some(name.to_string()),
                    count: 1,
                };
                if diagnostics.missing.contains(&selector) {
                    plan.notes.push(format!("{selector} is not in the index"));
                    plan.skips.push(declared(
                        "not_indexed",
                        "not indexed yet: no indexed note has it",
                    ));
                } else if diagnostics.left_out.contains(&selector) {
                    plan.notes.push(format!(
                        "{selector} is excluded by `leave out` or a built-in folder exclusion"
                    ));
                    plan.skips.push(declared(
                        "left_out",
                        "left out by `leave out` or a built-in exclusion",
                    ));
                }
                members.push(key(entity_type, name));
                if reading.include_who_links {
                    let expansion = diagnostics
                        .who_links
                        .iter()
                        .find(|d| d.entity == reading.entity)
                        .cloned();
                    if let Some(d) = &expansion {
                        members.extend(d.selected.iter().map(|e| key(&e.entity_type, &e.name)));
                        pattern_skips(&mut plan.skips, d, "who link", name, i, date);
                        if d.matched == 0 {
                            plan.notes.push("source has no indexed who links".into());
                        } else if d.explicit > 0 {
                            plan.notes.push(format!(
                                "{} who link(s) have their own reading, which takes precedence",
                                d.explicit
                            ));
                        }
                    }
                    plan.who_links = expansion;
                }
                if kind == "folder" {
                    let folder = name.to_lowercase();
                    let expansion = diagnostics.folders.get(&folder).cloned();
                    let children = selection
                        .expandable_folders
                        .get(&folder)
                        .cloned()
                        .unwrap_or_default();
                    let explicit: Vec<&String> = children
                        .iter()
                        .filter(|c| spec.reading(c, "link").is_some())
                        .collect();
                    if !explicit.is_empty() {
                        plan.skips.push(candidates(
                            i,
                            "own_reading",
                            format!("{} linked page(s) have their own reading", explicit.len()),
                            explicit.len(),
                        ));
                        plan.notes.push(format!(
                            "{} linked page(s) have their own reading, which takes precedence: {}",
                            explicit.len(),
                            explicit
                                .iter()
                                .map(|s| s.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                    }
                    members.extend(
                        children
                            .iter()
                            .filter(|c| spec.reading(c, "link").is_none())
                            .map(|c| key("link", c)),
                    );
                    if let Some(e) = &expansion {
                        folder_notes(&mut plan.notes, e, reading, as_of_declared);
                        folder_skips(&mut plan.skips, e, reading, name, i, date);
                    }
                    plan.expansion = expansion;
                }
            }
            let members: HashSet<_> = members.into_iter().collect();
            plan.jobs = jobs
                .jobs
                .iter()
                .filter(|j| members.contains(&key(&j.entity_type, &j.entity_name)))
                .cloned()
                .collect();
            plan.skipped = jobs
                .skipped
                .iter()
                .filter(|j| members.contains(&key(&j.entity_type, &j.entity_name)))
                .cloned()
                .collect();
            for skip in &plan.skipped {
                plan.notes.push(format!(
                    "{} {}: {}",
                    skip.entity_type,
                    skip.entity_name,
                    skip_text(&skip.reason, as_of_declared)
                ));
            }
            plan
        })
        .collect()
}

fn candidates(reading: usize, code: &'static str, reason: String, count: usize) -> Skip {
    Skip {
        scope: "candidates",
        code: SkipCode { code, reason },
        reading: Some(reading),
        kind: None,
        name: None,
        count,
    }
}

/// "on or before 2024-10-01" for a declared `as of`, else "before today".
fn before(date: Option<&str>) -> String {
    date.map_or_else(|| "before today".into(), |d| format!("on or before {d}"))
}

/// Linked pages a folder reading did not expand into, by gate. Silent for a
/// plain folder reading whose pages were never candidates.
fn folder_skips(
    skips: &mut Vec<Skip>,
    e: &FolderExpansion,
    r: &Reading,
    folder: &str,
    i: usize,
    date: Option<&str>,
) {
    if e.linked_pages == 0 {
        if r.include_linked_pages {
            skips.push(Skip {
                scope: "reading",
                code: SkipCode {
                    code: "no_linked_pages",
                    reason: "no page in this folder is linked from your notes".into(),
                },
                reading: Some(i),
                kind: Some("folder".into()),
                name: Some(folder.to_string()),
                count: 1,
            });
        }
        return;
    }
    // A plain folder reading only expands when dense enough; its pages'
    // frequency and activity gates are not the user's concern.
    let expanding = r.include_linked_pages;
    if expanding && e.below_frequency > 0 {
        skips.push(candidates(
            i,
            "below_link_frequency",
            format!(
                "{} linked page(s) are linked fewer than {} times",
                e.below_frequency,
                enzyme_core::db::repository::PAGE_LINK_MIN_FREQUENCY
            ),
            e.below_frequency,
        ));
    }
    if expanding && e.inactive > 0 {
        let year = date.map_or_else(
            || "the past year".into(),
            |d| format!("the year before {d}"),
        );
        skips.push(candidates(
            i,
            "inactive",
            format!("{} linked page(s) have no activity in {year}", e.inactive),
            e.inactive,
        ));
    }
    if e.below_density {
        skips.push(candidates(
            i,
            "below_density_gate",
            format!(
                "{} linked page(s) not expanded: too few for the folder's size; add `including linked pages`",
                e.eligible
            ),
            e.eligible,
        ));
    } else if e.eligible > e.selected.len() {
        let n = e.eligible - e.selected.len();
        skips.push(candidates(
            i,
            "cap_reached",
            format!("{n} more linked page(s) qualified, but the reading's limit was reached"),
            n,
        ));
    }
}

/// Matches a `tags|links matching` or `including who links` reading did not
/// select.
fn pattern_skips(
    skips: &mut Vec<Skip>,
    d: &PatternDiagnostics,
    kind: &str,
    name: &str,
    i: usize,
    date: Option<&str>,
) {
    if d.matched == 0 {
        skips.push(Skip {
            scope: "reading",
            code: SkipCode {
                code: "no_matches",
                reason: format!("no indexed {kind} matches {name:?}"),
            },
            reading: Some(i),
            kind: Some(kind.replace(' ', "_")),
            name: Some(name.to_string()),
            count: 1,
        });
        return;
    }
    if d.left_out > 0 {
        skips.push(candidates(
            i,
            "left_out",
            format!("{} match(es) left out by `leave out`", d.left_out),
            d.left_out,
        ));
    }
    if d.explicit > 0 {
        skips.push(candidates(
            i,
            "own_reading",
            format!("{} match(es) have their own reading", d.explicit),
            d.explicit,
        ));
    }
    if d.undated > 0 {
        skips.push(candidates(
            i,
            "no_dated_evidence",
            format!(
                "{} match(es) have no notes dated {}",
                d.undated,
                before(date)
            ),
            d.undated,
        ));
    }
    if d.eligible > d.selected.len() {
        let n = d.eligible - d.selected.len();
        skips.push(candidates(
            i,
            "cap_reached",
            format!("{n} more match(es) qualified, but the reading's limit was reached"),
            n,
        ));
    }
}

/// Why a selected entity gets no generation job, as a code and in brief.
fn skip_code(reason: &SkipReason, date: Option<&str>) -> SkipCode {
    let (code, reason) = match reason {
        SkipReason::NoOccurrences => (
            "no_occurrences",
            "not mentioned in any indexed note".to_string(),
        ),
        SkipReason::NoEvidenceBeforeReference { occurrences } => (
            "no_dated_evidence",
            format!(
                "too few dated notes: none dated {} ({occurrences} later)",
                before(date)
            ),
        ),
        SkipReason::NoUsableEvidence { occurrences } => (
            "no_usable_evidence",
            format!("its {occurrences} mention(s) gave no usable excerpt"),
        ),
        SkipReason::ThinContext {
            documents,
            tokens,
            min_documents,
            min_tokens,
        } => (
            "thin_context",
            format!(
                "too little material: {documents} note(s), {tokens} tokens; needs at least {min_documents} and {min_tokens}"
            ),
        ),
    };
    SkipCode { code, reason }
}

fn entity_type_of(kind: &str) -> &str {
    if kind == "log" { "append_log" } else { kind }
}

fn kind_of(entity_type: &str) -> &str {
    if entity_type == "append_log" {
        "log"
    } else {
        entity_type
    }
}

/// Every selected entity with its origin and status, and every skip with
/// its reason code. Origins resolve in precedence order: a declared reading,
/// then a reading's matches or expansion, then automatic selection.
fn explain(
    readings: &[ReadingPlan],
    selection: &SelectionResult,
    jobs: &JobsPlan,
    date: Option<&str>,
) -> (Vec<EntityPlan>, Vec<Skip>) {
    type Origin = (&'static str, Option<usize>, Option<String>);
    let mut origins: std::collections::HashMap<(String, String), Origin> = Default::default();
    let mut claim = |k: (String, String), origin: Origin| {
        origins.entry(k).or_insert(origin);
    };
    for (i, r) in readings.iter().enumerate() {
        if !r.pattern {
            claim(
                key(entity_type_of(&r.kind), &r.name),
                ("reading", Some(i), None),
            );
        }
    }
    let parent_of = |name: &str| {
        let name = name.to_lowercase();
        selection
            .expandable_folders
            .iter()
            .filter(|(_, children)| children.iter().any(|c| c.to_lowercase() == name))
            .map(|(folder, _)| folder.clone())
            .min()
    };
    for (i, r) in readings.iter().enumerate() {
        for e in r.matches.iter().flat_map(|d| &d.selected) {
            claim(key(&e.entity_type, &e.name), ("pattern", Some(i), None));
        }
        for e in r.who_links.iter().flat_map(|d| &d.selected) {
            claim(key(&e.entity_type, &e.name), ("who_link", Some(i), None));
        }
        if r.kind == "folder" && !r.pattern {
            let folder = r.name.to_lowercase();
            for child in selection
                .expandable_folders
                .get(&folder)
                .into_iter()
                .flatten()
            {
                claim(
                    key("link", child),
                    ("expanded", Some(i), Some(r.name.clone())),
                );
            }
        }
    }

    let planned: std::collections::HashMap<_, _> = jobs
        .jobs
        .iter()
        .map(|j| {
            (
                key(&j.entity_type, &j.entity_name),
                j.shape.requested_questions,
            )
        })
        .collect();
    let skipped: std::collections::HashMap<_, _> = jobs
        .skipped
        .iter()
        .map(|j| (key(&j.entity_type, &j.entity_name), &j.reason))
        .collect();
    let mut skips: Vec<Skip> = Vec::new();
    let entities: Vec<EntityPlan> = selection
        .entities
        .iter()
        .map(|entity| {
            let entity_type = entity.entity_type.as_str();
            let k = key(entity_type, &entity.name);
            let (origin, reading, expanded_from) = origins.get(&k).cloned().unwrap_or_else(|| {
                let parent = (entity_type == "link")
                    .then(|| parent_of(&entity.name))
                    .flatten();
                match (selection.is_automatic(&entity.name, entity_type), parent) {
                    (true, Some(parent)) => ("expanded", None, Some(parent)),
                    (true, None) => ("automatic", None, None),
                    (false, _) => ("reading", None, None),
                }
            });
            let (status, questions, skip) = match (planned.get(&k), skipped.get(&k)) {
                (Some(&questions), _) => ("planned", questions, None),
                (None, Some(reason)) => ("skipped", 0, Some(skip_code(reason, date))),
                (None, None) => (
                    "skipped",
                    0,
                    Some(SkipCode {
                        code: "no_job",
                        reason: "no generation job was built".into(),
                    }),
                ),
            };
            if let Some(code) = &skip {
                skips.push(Skip {
                    scope: "entity",
                    code: code.clone(),
                    reading,
                    kind: Some(kind_of(entity_type).to_string()),
                    name: Some(entity.name.clone()),
                    count: 1,
                });
            }
            EntityPlan {
                kind: kind_of(entity_type).to_string(),
                name: entity.name.clone(),
                origin,
                reading,
                expanded_from,
                status,
                questions,
                skip,
            }
        })
        .collect();

    let mut all: Vec<Skip> = readings
        .iter()
        .flat_map(|r| r.skips.iter().cloned())
        .collect();
    all.extend(skips);
    if let Some(a) = &selection.diagnostics.automatic
        && a.slots == 0
    {
        all.push(Skip {
            scope: "automatic",
            code: SkipCode {
                code: "cap_reached",
                reason: format!(
                    "your readings fill all {} slots, so nothing is picked automatically",
                    a.limit
                ),
            },
            reading: None,
            kind: None,
            name: None,
            count: 0,
        });
    }
    let rank = |s: &Skip| match s.scope {
        "reading" => 0,
        "entity" => 1,
        "candidates" => 2,
        _ => 3,
    };
    all.sort_by_key(rank);
    (entities, all)
}

fn count(entities: &[EntityPlan], skipped: &[Skip]) -> Counts {
    let mut counts = Counts {
        selected: entities.len(),
        planned: entities.iter().filter(|e| e.status == "planned").count(),
        ..Default::default()
    };
    counts.skipped = counts.selected - counts.planned;
    for e in entities {
        *counts.by_origin.entry(e.origin).or_default() += 1;
    }
    counts.candidates_skipped = skipped
        .iter()
        .filter(|s| s.scope != "entity")
        .map(|s| s.count)
        .sum();
    counts
}

fn source_text(r: &Reading) -> String {
    let (kind, name) = enzyme_spec::split_entity(&r.entity);
    let mut s = if r.pattern {
        format!("{kind}s matching {name:?}")
    } else {
        format!("{kind} {name:?}")
    };
    if r.include_linked_pages {
        s.push_str(" including linked pages");
    }
    if r.include_who_links {
        s.push_str(" including who links");
    }
    s
}

fn folder_notes(notes: &mut Vec<String>, e: &FolderExpansion, r: &Reading, as_of: bool) {
    if !e.selected.is_empty() {
        return;
    }
    if e.linked_pages == 0 {
        if r.include_linked_pages {
            notes.push(
                "no linked pages: no direct child page of this folder is linked as [[name]] from notes"
                    .into(),
            );
        }
    } else if e.eligible == 0 {
        notes.push(format!(
            "no eligible linked pages: {} linked fewer than 3 times, {} with no activity within 365 days of the reference instant{}",
            e.below_frequency,
            e.inactive,
            if e.inactive == 0 {
                String::new()
            } else if as_of {
                " (trends are cached at index time; run `enzyme refresh` after changing `as of`)"
                    .into()
            } else {
                "; for a historical corpus add `as of latest evidence` or `as of \"YYYY-MM-DD\"`"
                    .into()
            }
        ));
    } else if e.below_density {
        notes.push(format!(
            "{} eligible linked page(s), but density {:.3} is below the auto-expansion threshold {EXPANDABLE_DENSITY_THRESHOLD}; add `including linked pages`",
            e.eligible, e.density
        ));
    }
}

fn pattern_notes(notes: &mut Vec<String>, d: &PatternDiagnostics, kind: &str, as_of: bool) {
    if d.matched == 0 {
        notes.push(format!("pattern matched no indexed {kind}s"));
        return;
    }
    if d.explicit > 0 {
        notes.push(format!(
            "{} match(es) have their own reading, which takes precedence",
            d.explicit
        ));
    }
    if d.eligible == 0 {
        let mut parts = Vec::new();
        if d.left_out > 0 {
            parts.push(format!("{} excluded by `leave out {kind}s`", d.left_out));
        }
        if d.undated > 0 {
            parts.push(format!(
                "{} with no evidence at or before the reference instant{}",
                d.undated,
                if as_of { "" } else { " (future-dated only)" }
            ));
        }
        notes.push(format!(
            "{} matched, none eligible{}",
            d.matched,
            if parts.is_empty() {
                String::new()
            } else {
                format!(": {}", parts.join(", "))
            }
        ));
    } else if d.left_out > 0 {
        notes.push(format!("{} excluded by `leave out {kind}s`", d.left_out));
    }
}

fn skip_text(reason: &SkipReason, as_of: bool) -> String {
    match reason {
        SkipReason::NoOccurrences => "no job: no occurrences in the index".into(),
        SkipReason::NoEvidenceBeforeReference { occurrences } => format!(
            "no job: no dated evidence at or before the reference instant ({occurrences} later occurrence(s)){}",
            if as_of { "" } else { "; consider `as of`" }
        ),
        SkipReason::NoUsableEvidence { occurrences } => {
            format!("no job: {occurrences} occurrence(s) in scope yielded no usable excerpt")
        }
        SkipReason::ThinContext {
            documents,
            tokens,
            min_documents,
            min_tokens,
        } => format!(
            "no job: thin context ({documents} document(s), {tokens} tokens; needs {min_documents} and {min_tokens})"
        ),
    }
}

/// Effective learning and where each field came from. Readings store the
/// resolved (inherited) policy; the unresolved AST tells which scope set it.
/// A budget equal to the vault's is its volume-allocation base, not a fixed
/// count, exactly as job construction treats it.
fn learning_plan(
    r: &Reading,
    raw: Option<&Reading>,
    vault: &enzyme_spec::Vault,
    globals: &[Learning],
    vault_budget: Option<usize>,
) -> LearningPlan {
    let scopes: [(&'static str, Vec<&Learning>); 3] = [
        ("reading", raw.map(|r| &r.learning).into_iter().collect()),
        ("vault", vec![&vault.learning]),
        ("global", globals.iter().collect()),
    ];
    let from = |set: &dyn Fn(&Learning) -> bool| {
        scopes
            .iter()
            .find(|(_, ls)| ls.iter().any(|l| set(l)))
            .map_or("default", |(name, _)| *name)
    };
    let l = &r.learning;
    let expands = r.pattern || r.include_linked_pages || r.entity.starts_with("folder:");
    LearningPlan {
        sampling: Sourced {
            value: l
                .sampling_mode()
                .map_or("legacy selector".into(), |m| format!("by {m}")),
            from: from(&|l| l.sample.is_some() || l.favor.is_some()),
        },
        periods: Sourced {
            value: l.periods.map_or("auto", |p| p.as_str()).to_string(),
            from: from(&|l| l.periods.is_some()),
        },
        budget: Sourced {
            value: match l.budget {
                None => "volume allocation".into(),
                Some(b) if l.budget == vault_budget => format!("volume allocation, base {b}"),
                Some(b) => b.to_string(),
            },
            from: from(&|l| l.budget.is_some()),
        },
        select: Sourced {
            value: match (&l.selection, expands) {
                (_, false) => "n/a".into(),
                (None, true) => format!("legacy ranking, up to {MAX_FOLDER_CHILDREN}"),
                (Some(s), true) => {
                    let by = match s.by {
                        SelectionMode::Frequency => "frequency",
                        SelectionMode::Recency => "recency",
                    };
                    match s.amount {
                        SelectionAmount::Count { count } => format!("{count} by {by}"),
                        SelectionAmount::Percent { percent, up_to } => {
                            let cap = up_to.map_or(String::new(), |n| format!(" up to {n}"));
                            format!("{percent}% by {by}{cap}")
                        }
                    }
                }
            },
            from: from(&|l| l.selection.is_some()),
        },
        as_of: Sourced {
            value: match &l.as_of {
                None => "now".into(),
                Some(AsOf::Date { date }) => format!("{date:?}"),
                Some(AsOf::LatestEvidence) => "latest evidence".into(),
            },
            from: from(&|l| l.as_of.is_some()),
        },
    }
}

fn render(plan: &VaultPlan) -> String {
    let mut s = format!("vault {}\n", plan.vault);
    if let Some(declared) = &plan.declared_vault {
        s.push_str(&format!("  declared as  {declared}\n"));
    }
    s.push_str(&format!(
        "  reference    {} ({})\n  prompts      {}\n",
        plan.reference.instant, plan.reference.source, plan.prompts
    ));
    match &plan.automatic {
        Some(a) => {
            let up_to = a.up_to.map_or_else(String::new, |n| format!(", up to {n}"));
            s.push_str(&format!(
                "  selection    {} (learn questions automatically: {} declared of limit {}{up_to} → {} automatic slots)\n",
                plan.selection, a.declared, a.limit, a.slots
            ));
        }
        None if plan.selection == "automatic" => {
            s.push_str("  selection    automatic (no readings declared)\n");
        }
        None => {}
    }
    for (i, r) in plan.readings.iter().enumerate() {
        s.push_str(&format!("\n{}. {}  about {}\n", i + 1, r.source, r.profile));
        let l = &r.learning;
        let sampling = if l.sampling.value == "legacy selector" {
            format!("legacy sampling ({})", l.sampling.from)
        } else {
            format!("sample {} ({})", l.sampling.value, l.sampling.from)
        };
        let mut policy = vec![
            sampling,
            format!("budget {} ({})", l.budget.value, l.budget.from),
        ];
        if l.sampling.value != "legacy selector" {
            policy.insert(
                1,
                format!("periods {} ({})", l.periods.value, l.periods.from),
            );
        }
        if l.select.value != "n/a" {
            policy.push(format!("select {} ({})", l.select.value, l.select.from));
        }
        if l.as_of.value != "now" {
            policy.push(format!("as of {} ({})", l.as_of.value, l.as_of.from));
        }
        s.push_str(&format!("   learning   {}\n", policy.join(" · ")));
        if let Some(e) = &r.expansion {
            if e.linked_pages > 0 || r.including_linked_pages {
                s.push_str(&format!(
                    "   pages      {} linked · {} below frequency 3 · {} inactive · {} eligible → {} selected\n",
                    e.linked_pages,
                    e.below_frequency,
                    e.inactive,
                    e.eligible,
                    e.selected.len()
                ));
                s.push_str(&scores(&e.selected));
            }
        }
        if let Some(d) = &r.matches {
            s.push_str(&format!(
                "   matches    {} matched · {} left out · {} explicit · {} undated · {} eligible → {} selected\n",
                d.matched,
                d.left_out,
                d.explicit,
                d.undated,
                d.eligible,
                d.selected.len()
            ));
            s.push_str(&scores(&d.selected));
        }
        if let Some(d) = &r.who_links {
            s.push_str(&format!("   who links  {} matched · {} left out · {} explicit · {} undated · {} eligible → {} selected\n",
                d.matched, d.left_out, d.explicit, d.undated, d.eligible, d.selected.len()));
            s.push_str(&scores(&d.selected));
        }
        s.push_str(&jobs_text(&r.jobs));
        for note in &r.notes {
            s.push_str(&format!("   ! {note}\n"));
        }
    }
    if let Some(a) = &plan.automatic {
        s.push_str(&format!(
            "\nautomatic selection  {} picked · {} linked pages of picked folders\n",
            a.picked.len(),
            a.expanded.len()
        ));
        for e in &a.picked {
            s.push_str(&format!("   picked     {} {}\n", e.entity_type, e.name));
        }
        for e in &a.expanded {
            s.push_str(&format!("   expanded   {} {}\n", e.entity_type, e.name));
        }
        if plan.other_jobs.is_empty() && plan.other_skipped.is_empty() {
            s.push_str("   jobs       none\n");
        }
    }
    if !plan.other_jobs.is_empty() || !plan.other_skipped.is_empty() {
        if plan.automatic.is_none() {
            s.push_str("\nother selected entities\n");
        }
        s.push_str(&jobs_text(&plan.other_jobs));
        for skip in &plan.other_skipped {
            s.push_str(&format!(
                "   ! {} {}: {}\n",
                skip.entity_type,
                skip.entity_name,
                skip_text(&skip.reason, plan.reference.source != "wall clock")
            ));
        }
    }
    let c = &plan.counts;
    let origins: Vec<String> = c
        .by_origin
        .iter()
        .map(|(origin, n)| format!("{n} {origin}"))
        .collect();
    s.push_str(&format!(
        "\nentities  {} selected ({}) → {} planned, {} skipped · {} other candidates skipped\n",
        c.selected,
        origins.join(" · "),
        c.planned,
        c.skipped,
        c.candidates_skipped
    ));
    s.push_str(&format!(
        "total  {} jobs · {} requested questions · {} excerpts · {} skipped\n",
        plan.totals.jobs,
        plan.totals.requested_questions,
        plan.totals.excerpts,
        plan.totals.skipped
    ));
    s
}

fn scores(selected: &[enzyme_core::pipeline::selection_steps::ScoredEntity]) -> String {
    let width = selected.iter().map(|e| e.name.len()).max().unwrap_or(0);
    selected
        .iter()
        .map(|e| {
            format!(
                "     {:<width$}  docs {:>3}  recency {:.2}\n",
                e.name, e.documents, e.recency
            )
        })
        .collect()
}

fn jobs_text(jobs: &[PlannedJob]) -> String {
    if jobs.is_empty() {
        return "   jobs       none\n".into();
    }
    let labels: Vec<String> = jobs
        .iter()
        .map(|j| format!("{} {}", j.entity_type, j.entity_name))
        .collect();
    let width = labels.iter().map(String::len).max().unwrap_or(0);
    let mut s = String::new();
    for (j, label) in jobs.iter().zip(&labels) {
        let shape = &j.shape;
        let budget = if shape.byte_budget == 0 {
            "unlimited".to_string()
        } else {
            shape.byte_budget.to_string()
        };
        s.push_str(&format!(
            "   job        {label:<width$}  {:>2} questions  {:>2}/{} excerpts  {}/{} bytes  {} sources\n",
            shape.requested_questions,
            shape.excerpts,
            shape.excerpt_limit,
            shape.bytes,
            budget,
            shape.sources
        ));
        if !shape.periods.is_empty() {
            let periods: Vec<String> = shape
                .periods
                .iter()
                .map(|p| format!("{} {}x/{}q", p.label, p.excerpts, p.questions))
                .collect();
            s.push_str(&format!(
                "              {}periods  {}\n",
                shape
                    .sampling
                    .as_ref()
                    .map_or(String::new(), |m| format!("by {m} · ")),
                periods.join("  ")
            ));
        }
    }
    s
}

/// One file per job with the shared system prompt and that job's user prompt,
/// from the same job construction `dump-prompts` uses.
fn write_prompts(dir: &Path, plans: &[(VaultPlan, JobsPlan)]) -> Result<usize> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let mut written = 0;
    for (v, (_, jobs)) in plans.iter().enumerate() {
        for (i, job) in jobs.jobs.iter().enumerate() {
            let slug: String = job
                .entity_name
                .chars()
                .map(|c| {
                    if c.is_alphanumeric() || c == '-' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            let prefix = if plans.len() > 1 {
                format!("{}-", v + 1)
            } else {
                String::new()
            };
            let path = dir.join(format!(
                "{prefix}{:03}-{}-{slug}.txt",
                i + 1,
                job.entity_type
            ));
            std::fs::write(
                &path,
                format!(
                    "=== system ===\n{}\n\n=== user ===\n{}\n",
                    jobs.system_prompt, job.user_prompt
                ),
            )
            .with_context(|| format!("write {}", path.display()))?;
            written += 1;
        }
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn program(source: &str) -> (Program, Program) {
        let ast = enzyme_spec::parse(source).unwrap();
        let resolved = enzyme_spec::resolve(vec![ast.clone()], Path::new("/home/test")).unwrap();
        (ast, resolved)
    }

    #[test]
    fn learning_provenance_names_the_scope_that_set_each_field() {
        let (ast, resolved) = program(
            r#"learning { question budget 5 }
vault "/notes" {
  learning { sample by time as of "2024-10-01" }
  learn questions from folder "people" including linked pages { periods monthly select 2 by frequency }
  learn questions from tag "craft"
}"#,
        );
        let globals = vec![ast.learning.clone()];
        let v = &resolved.vaults[0];
        let budget = v.learning.budget;
        let folder = learning_plan(
            &v.readings[0],
            ast.vaults[0].readings.first(),
            &ast.vaults[0],
            &globals,
            budget,
        );
        assert_eq!(
            (folder.sampling.value.as_str(), folder.sampling.from),
            ("by time", "vault")
        );
        assert_eq!(
            (folder.periods.value.as_str(), folder.periods.from),
            ("monthly", "reading")
        );
        assert_eq!(
            (folder.budget.value.as_str(), folder.budget.from),
            ("volume allocation, base 5", "global")
        );
        assert_eq!(
            (folder.select.value.as_str(), folder.select.from),
            ("2 by frequency", "reading")
        );
        assert_eq!(folder.as_of.from, "vault");
        let tag = learning_plan(
            &v.readings[1],
            ast.vaults[0].readings.get(1),
            &ast.vaults[0],
            &globals,
            budget,
        );
        assert_eq!(tag.select.value, "n/a");
        assert_eq!(
            (tag.periods.value.as_str(), tag.periods.from),
            ("auto", "default")
        );
    }

    #[test]
    fn drop_reasons_explain_empty_expansions_and_patterns() {
        let (_, resolved) = program(
            r#"vault "/notes" { learn questions from folder "people" including linked pages }"#,
        );
        let reading = &resolved.vaults[0].readings[0];
        let mut notes = Vec::new();
        let inactive = FolderExpansion {
            linked_pages: 4,
            below_frequency: 1,
            inactive: 3,
            ..Default::default()
        };
        folder_notes(&mut notes, &inactive, reading, false);
        assert!(
            notes[0].contains("365 days") && notes[0].contains("as of"),
            "{notes:?}"
        );
        notes.clear();
        folder_notes(&mut notes, &FolderExpansion::default(), reading, false);
        assert!(notes[0].starts_with("no linked pages"));

        notes.clear();
        pattern_notes(&mut notes, &PatternDiagnostics::default(), "tag", false);
        assert_eq!(notes, ["pattern matched no indexed tags"]);
        notes.clear();
        let d = PatternDiagnostics {
            matched: 3,
            left_out: 1,
            explicit: 1,
            undated: 1,
            ..Default::default()
        };
        pattern_notes(&mut notes, &d, "tag", true);
        assert!(notes[0].contains("own reading"));
        assert!(notes[1].contains("1 excluded by `leave out tags`"));
        assert!(notes[1].contains("no evidence at or before the reference instant"));
        assert!(
            skip_text(
                &SkipReason::NoEvidenceBeforeReference { occurrences: 2 },
                false
            )
            .contains("consider `as of`")
        );
    }

    #[test]
    fn skip_codes_name_each_gate_with_a_human_reason() {
        let (_, resolved) = program(
            r#"vault "/notes" {
  learn questions from folder "people" including linked pages
  learn questions from folder "daily"
}"#,
        );
        let [including, plain] = &resolved.vaults[0].readings[..] else {
            panic!()
        };
        let gated = FolderExpansion {
            linked_pages: 9,
            below_frequency: 4,
            inactive: 2,
            eligible: 3,
            ..Default::default()
        };
        let mut skips = Vec::new();
        folder_skips(
            &mut skips,
            &gated,
            including,
            "people",
            0,
            Some("2024-10-01"),
        );
        let codes: Vec<_> = skips.iter().map(|s| (s.code.code, s.count)).collect();
        assert_eq!(
            codes,
            [
                ("below_link_frequency", 4),
                ("inactive", 2),
                ("cap_reached", 3)
            ]
        );
        assert!(skips[1].code.reason.contains("2024-10-01"));
        assert!(
            skips
                .iter()
                .all(|s| s.scope == "candidates" && s.reading == Some(0))
        );

        // A plain folder reading reports only the density gate it can act on.
        skips.clear();
        let sparse = FolderExpansion {
            below_density: true,
            ..gated.clone()
        };
        folder_skips(&mut skips, &sparse, plain, "daily", 1, None);
        assert_eq!(skips.len(), 1);
        assert_eq!(skips[0].code.code, "below_density_gate");
        assert!(skips[0].code.reason.contains("including linked pages"));
        skips.clear();
        folder_skips(
            &mut skips,
            &FolderExpansion::default(),
            including,
            "people",
            0,
            None,
        );
        assert_eq!(
            (skips[0].scope, skips[0].code.code),
            ("reading", "no_linked_pages")
        );

        skips.clear();
        pattern_skips(
            &mut skips,
            &PatternDiagnostics::default(),
            "tag",
            "zz-*",
            2,
            None,
        );
        assert_eq!(skips[0].code.code, "no_matches");
        assert_eq!(skips[0].name.as_deref(), Some("zz-*"));
        skips.clear();
        let d = PatternDiagnostics {
            matched: 6,
            left_out: 1,
            explicit: 1,
            undated: 2,
            eligible: 2,
            ..Default::default()
        };
        pattern_skips(&mut skips, &d, "tag", "a*", 0, None);
        let codes: Vec<_> = skips.iter().map(|s| s.code.code).collect();
        assert_eq!(
            codes,
            [
                "left_out",
                "own_reading",
                "no_dated_evidence",
                "cap_reached"
            ]
        );
        assert!(skips[2].code.reason.contains("before today"));

        let thin = skip_code(
            &SkipReason::ThinContext {
                documents: 1,
                tokens: 30,
                min_documents: 2,
                min_tokens: 32,
            },
            None,
        );
        assert_eq!(thin.code, "thin_context");
        assert!(
            thin.reason.contains("needs at least 2 and 32"),
            "{}",
            thin.reason
        );
        assert_eq!(
            skip_code(&SkipReason::NoOccurrences, None).code,
            "no_occurrences"
        );
        assert_eq!(
            skip_code(
                &SkipReason::NoEvidenceBeforeReference { occurrences: 3 },
                Some("2024-01-01")
            )
            .code,
            "no_dated_evidence"
        );
    }

    #[test]
    fn source_text_reads_like_the_declaration() {
        let (_, resolved) = program(
            r#"vault "/notes" {
  learn questions from folder "people" including linked pages
  learn questions from tags matching "auto-*"
  learn questions from log "journal"
}"#,
        );
        let texts: Vec<_> = resolved.vaults[0]
            .readings
            .iter()
            .map(source_text)
            .collect();
        assert_eq!(
            texts,
            [
                r#"folder "people" including linked pages"#,
                r#"tags matching "auto-*""#,
                r#"log "journal""#
            ]
        );
    }
}
