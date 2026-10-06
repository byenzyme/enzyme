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
    pub file: &'a Path,
    pub vault: Option<&'a Path>,
    pub json: bool,
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

/// Resolve the file like the runtime would and plan each selected vault.
fn plan(options: &PlanOptions<'_>) -> Result<Vec<(VaultPlan, JobsPlan)>> {
    let file = options.file;
    let source =
        std::fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
    let ast = enzyme_spec::parse(&source).with_context(|| format!("parse {}", file.display()))?;
    let user_home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME missing")?;
    // A file inside the runtime configs directory shares its namespace (e.g.
    // profiles defined in profiles.enzyme), so resolve it with its siblings.
    // Another workspace's invalid file does not block planning this one.
    let mut others = Vec::new();
    if let Some(directory) = runtime_configs_directory(file)? {
        for entry in std::fs::read_dir(&directory)? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "enzyme")
                && path.is_file()
                && !same_file(&path, file)
                && let Ok(program) = enzyme_spec::parse(&std::fs::read_to_string(&path)?)
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
    match options.vault {
        Some(vault) => {
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
        None => {
            for (i, v) in resolved.vaults.iter().enumerate() {
                targets.push((i, PathBuf::from(&v.path), None));
            }
        }
    }

    let mut plans = Vec::new();
    for (index, vault_path, declared) in targets {
        let mut program = resolved.clone();
        let mut vault = program.vaults.swap_remove(index);
        if !vault.path.starts_with("workspace:") {
            vault.path = vault_path.to_string_lossy().to_string();
        }
        program.vaults = vec![vault];
        let raw = &raw_scopes[index];
        plans.push(plan_vault(
            &vault_path,
            declared,
            program,
            raw,
            &globals,
            options.local,
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

fn plan_vault(
    vault_path: &Path,
    declared_vault: Option<String>,
    program: Program,
    raw: &enzyme_spec::Vault,
    globals: &[Learning],
    local: bool,
) -> Result<(VaultPlan, JobsPlan)> {
    let paths = EnzymePaths::new(vault_path)?;
    anyhow::ensure!(
        paths.vault_db.is_file(),
        "{} is not indexed (no {}); run `enzyme -p {} init --stage index` first",
        vault_path.display(),
        paths.vault_db.display(),
        vault_path.display()
    );
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
    };
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
            };
            let mut members: Vec<(String, String)> = Vec::new();
            if reading.pattern {
                let found = patterns.next().cloned();
                if let Some(d) = &found {
                    members.extend(d.selected.iter().map(|e| key(&e.entity_type, &e.name)));
                    pattern_notes(&mut plan.notes, d, kind, as_of_declared);
                }
                plan.matches = found;
            } else {
                let selector = enzyme_spec::entity_selector(kind, name);
                if diagnostics.missing.contains(&selector) {
                    plan.notes.push(format!("{selector} is not in the index"));
                } else if diagnostics.left_out.contains(&selector) {
                    plan.notes.push(format!(
                        "{selector} is excluded by `leave out` or a built-in folder exclusion"
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
    s.push_str(&format!(
        "\ntotal  {} jobs · {} requested questions · {} excerpts · {} skipped\n",
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
