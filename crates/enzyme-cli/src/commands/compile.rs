use anyhow::{Context, Result, ensure};
use enzyme_core::config::{EnzymeConfig, EnzymePaths, enzyme_config::CatalyzedEntity};
use enzyme_spec::{Learning, MarkdownSource, Program, Reading, Source, Workspace};
use std::{
    collections::BTreeSet,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

/// Generate structured choices first; rendering never invokes a model.
pub async fn run(
    path: &Path,
    color: ColorChoice,
    output: Option<&Path>,
    verbose: bool,
    dry_run: bool,
    json: bool,
) -> Result<()> {
    let start = std::time::Instant::now();
    let path = path.canonicalize().context("resolve vault directory")?;
    ensure!(path.is_dir(), "vault must be a directory");
    if let Some(file) = output {
        ensure!(
            !file.exists(),
            "destination already exists: {}",
            file.display()
        );
    }
    let mut config = EnzymeConfig::load(&path)?.unwrap_or_default();
    let directory = EnzymePaths::enzyme_home()?.join("configs");
    let namespace = enzyme_spec::source_files(&directory)?;
    let snapshots: Vec<_> = namespace
        .iter()
        .map(|(file, _)| Ok((file.clone(), std::fs::read_to_string(file)?)))
        .collect::<Result<_>>()?;
    ensure!(
        config.sources.is_empty() && config.notes_path.is_none() && config.notes_roots.is_empty(),
        "compile supports a single Markdown vault; structured sources must keep their existing configuration"
    );
    let generated = super::scan::generate_jev_setup(&path, true).await?;
    ensure!(
        !generated.proposal.entities.is_empty(),
        "Jev selected no entities; no configuration generated"
    );
    for folder in generated.excluded_folders() {
        if !config.excluded_folders.contains(folder) {
            config.excluded_folders.push(folder.clone());
        }
    }
    let evidence = if json {
        Some(generated.evidence()?)
    } else {
        None
    };
    let temporal_policies = generated
        .proposal
        .decisions
        .iter()
        .filter(|d| d.selected)
        .map(|d| (d.spec.clone(), d.learning.clone()))
        .collect();
    config.catalyzed_entities = generated.proposal.entities;
    let program = structured_setup(&path, config, &temporal_policies)?;
    let source = render(&program);
    // Exercise the same parser/resolver as init before emitting any configuration.
    enzyme_spec::resolve_in(
        vec![enzyme_spec::parse(&source)?],
        &enzyme_core::config::default_spec_environment()?,
    )?;
    let saved = if dry_run {
        None
    } else if let Some(file) = output {
        let parent = file
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(source.as_bytes())?;
        temporary.as_file().sync_all()?;
        temporary
            .persist_noclobber(file)
            .context("save new proposal")?;
        Some(file.to_path_buf())
    } else {
        Some(save_setup(
            &path, &directory, &program, namespace, snapshots,
        )?)
    };
    let enabled = match color {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => {
            io::stdout().is_terminal()
                && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
                && std::env::var("TERM").as_deref() != Ok("dumb")
        }
    };
    // One machine-readable payload for setup agents: the program that was
    // written plus the inventory it was chosen from, so "seen but not read" is
    // answerable without a second scan.
    if let Some(evidence) = evidence {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "schema_version": "compile.v1",
                "vault_path": path.to_string_lossy(),
                "saved_to": saved.as_ref().map(|f| f.display().to_string()),
                "program": source,
                "evidence": evidence,
                "total_ms": start.elapsed().as_millis(),
            }))?
        );
        return Ok(());
    }
    if verbose || dry_run {
        io::stdout()
            .lock()
            .write_all(highlight(&source, enabled).as_bytes())?;
    }
    if let Some(file) = saved {
        eprintln!(
            "Saved proposal: {} ({:.2}s)",
            file.display(),
            start.elapsed().as_secs_f64()
        );
    }
    Ok(())
}

/// How a setup agent turns its answers into a program.
const PACKET_WRITING: &str = "Answer `questions` for every candidate yourself, as the selection model would (in a question, `candidates[0]` stands for each candidate in turn and `state` means `guidance`): apply `guidance` to each candidate's evidence, and also use what you can read directly, especially the user's AGENTS.md, CLAUDE.md, or router notes and the notes themselves. For each candidate you include, add its `statement` inside the skeleton's workspace block. Append `about <profile>` when you chose a profile, then `{ sample by time }` or `{ sample by recency }` when you chose a temporal policy, e.g. `learn questions from folder \"People\" including linked pages about relational { sample by time }`. Add leave-outs or readings the candidates miss when the user's own notes justify them. Write the program to `save_to`; if that file exists, replace only the workspace for this vault and keep everything else. Then check it with `enzyme spec compile <save_to>`.";

/// Print what selection would be asked, so a setup agent can answer it itself.
/// Local only: no index, login, or model call, and nothing is saved.
pub fn run_packet(path: &Path) -> Result<()> {
    let path = path.canonicalize().context("resolve vault directory")?;
    ensure!(path.is_dir(), "vault must be a directory");
    let mut config = EnzymeConfig::load(&path)?.unwrap_or_default();
    ensure!(
        config.sources.is_empty() && config.notes_path.is_none() && config.notes_roots.is_empty(),
        "compile supports a single Markdown vault; structured sources must keep their existing configuration"
    );
    let directory = EnzymePaths::enzyme_home()?.join("configs");
    let namespace = enzyme_spec::source_files(&directory)?;
    let prepared = super::scan::prepare_jev_setup(&path)?;
    ensure!(
        !prepared.candidates.is_empty(),
        "no Markdown candidates found; no packet generated"
    );
    for folder in prepared.excluded_folders() {
        if !config.excluded_folders.contains(folder) {
            config.excluded_folders.push(folder.clone());
        }
    }
    config.catalyzed_entities.clear();
    let mut skeleton = structured_setup(&path, config, &Default::default())?;
    let home = std::env::var_os("HOME").context("HOME missing")?;
    let location = locate(&path, &namespace, Path::new(&home));
    // Keep the names an existing declaration uses, as saving a proposal does.
    if let Some((file, true, position)) = location {
        let existing = &namespace[file].1.workspaces[position];
        let workspace = &mut skeleton.workspaces[0];
        workspace.name = existing.name.clone();
        if let (Some(Source::Markdown(old)), Some(Source::Markdown(new))) =
            (existing.sources.first(), workspace.sources.first_mut())
        {
            new.name = old.name.clone();
        }
    }
    let save_to = match location {
        Some((file, _, _)) => namespace[file].0.clone(),
        None => new_setup_file(&path, &directory),
    };
    let mut packet =
        enzyme_core::pipeline::jev_selection::JevSelectionPipeline::packet(&prepared.candidates, true)?;
    for candidate in packet["candidates"].as_array_mut().into_iter().flatten() {
        let spec = candidate["spec"].as_str().unwrap_or_default().to_owned();
        let expandable = candidate["expandable"].as_bool().unwrap_or(false);
        candidate["statement"] =
            reading_statement(&skeleton.workspaces[0], &spec, expandable)?.into();
    }
    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "schema_version": "compile-packet.v1",
            "vault_path": path.to_string_lossy(),
            "save_to": save_to.display().to_string(),
            "save_to_exists": save_to.exists(),
            "writing": PACKET_WRITING,
            "skeleton": render(&skeleton),
            "guidance": packet["guidance"].take(),
            "questions": packet["questions"].take(),
            "candidates": packet["candidates"].take(),
            "unevaluated_entities": packet["unevaluated_entities"].take(),
            "evidence": prepared.evidence()?,
        }))?
    );
    Ok(())
}

/// The canonical statement reading one candidate, rendered by the spec itself.
fn reading_statement(workspace: &Workspace, spec: &str, expandable: bool) -> Result<String> {
    let mut single = workspace.clone();
    single.readings = vec![Reading {
        entity: spec.to_owned(),
        profile: "auto".into(),
        definition: None,
        learning: Learning::default(),
        include_linked_pages: expandable,
        include_who_links: false,
        pattern: false,
    }];
    // The renderer continues a reading on deeper-indented lines; join them.
    let rendered = enzyme_spec::render_workspace(&single);
    let mut lines = rendered.lines().skip_while(|l| !l.trim_start().starts_with("learn questions"));
    let first = lines
        .next()
        .with_context(|| format!("render a reading for {spec}"))?;
    let mut statement = first.trim().to_owned();
    for line in lines.take_while(|l| l.starts_with("    ")) {
        statement.push(' ');
        statement.push_str(line.trim());
    }
    Ok(statement)
}

fn render(program: &Program) -> String {
    let source = enzyme_spec::render_program(program)
        .strip_prefix("// Enzyme reading configuration\n\n")
        .expect("renderer header")
        .to_owned();
    let mut described = BTreeSet::new();
    let mut rendered = String::with_capacity(source.len());
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let profile = trimmed
            .strip_prefix("about ")
            .and_then(|rest| rest.split_whitespace().next())
            .map(|key| key.trim_end_matches('{'))
            .and_then(enzyme_core::llm::get_profile);
        if let Some(profile) = profile.filter(|profile| described.insert(profile.key)) {
            let indent = &line[..line.len() - trimmed.len()];
            rendered.push_str(&format!(
                "{indent}// {} (`{}`)\n",
                profile.display_name, profile.key
            ));
            rendered.push_str(&render_comment(profile.user_description, indent));
            rendered.push_str(&format!(
                "{indent}// Change this `about` name if the material should be read through a different lens.\n"
            ));
        }
        rendered.push_str(line);
    }
    rendered
}

fn render_comment(text: &str, indent: &str) -> String {
    const WIDTH: usize = 80;
    let mut rendered = String::new();
    let prefix = format!("{indent}// ");
    let mut line = prefix.clone();
    for word in text.split_whitespace() {
        if line.len() > prefix.len() && line.len() + 1 + word.len() > WIDTH {
            rendered.push_str(&line);
            rendered.push('\n');
            line.clone_from(&prefix);
        }
        if line.len() > prefix.len() {
            line.push(' ');
        }
        line.push_str(word);
    }
    rendered.push_str(&line);
    rendered.push('\n');
    rendered
}

/// Replace only this vault's setup while retaining sibling vaults, shared settings,
/// and unused profile definitions in its file. Validate the complete namespace.
fn save_setup(
    path: &Path,
    directory: &Path,
    generated: &Program,
    mut namespace: Vec<(PathBuf, Program)>,
    snapshots: Vec<(PathBuf, String)>,
) -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME missing")?;
    let user_home = Path::new(&home);
    let location = locate(path, &namespace, user_home);
    let index = if let Some((index, is_workspace, position)) = location {
        if is_workspace {
            let existing = &namespace[index].1.workspaces[position];
            let mut replacement = generated.workspaces[0].clone();
            replacement.name = existing.name.clone();
            if let (Some(Source::Markdown(old)), Some(Source::Markdown(new))) =
                (existing.sources.first(), replacement.sources.first_mut())
            {
                new.name = old.name.clone();
            }
            namespace[index].1.workspaces[position] = replacement;
        } else {
            namespace[index].1.vaults.remove(position);
            namespace[index]
                .1
                .workspaces
                .push(generated.workspaces[0].clone());
        }
        namespace[index]
            .1
            .profiles
            .extend(generated.profiles.clone());
        index
    } else {
        let file = new_setup_file(path, directory);
        ensure!(
            !file.exists(),
            "destination already exists: {}",
            file.display()
        );
        namespace.push((file, generated.clone()));
        namespace.len() - 1
    };
    enzyme_spec::resolve_in(
        namespace.iter().map(|(_, p)| p.clone()).collect(),
        &enzyme_core::config::spec_environment(
            user_home,
            &enzyme_core::config::EnzymePaths::enzyme_home()?,
        )?,
    )?;
    for (file, previous) in snapshots {
        ensure!(
            std::fs::read_to_string(&file)? == previous,
            "configuration changed during generation: {}",
            file.display()
        );
    }
    let (file, program) = &namespace[index];
    enzyme_spec::atomic_write(file, &render(program))?;
    Ok(file.clone())
}

/// The file and position already declaring this vault: (file, is workspace, index).
fn locate(
    path: &Path,
    namespace: &[(PathBuf, Program)],
    user_home: &Path,
) -> Option<(usize, bool, usize)> {
    namespace
        .iter()
        .enumerate()
        .find_map(|(file_index, (_, program))| {
            let matches_path = |raw: &str| {
                let expanded = if raw == "~" {
                    user_home.to_path_buf()
                } else if let Some(p) = raw.strip_prefix("~/") {
                    user_home.join(p)
                } else {
                    PathBuf::from(raw)
                };
                expanded.canonicalize().unwrap_or(expanded) == path
            };
            program
                .workspaces
                .iter()
                .position(|workspace| workspace.markdown_path().is_some_and(matches_path))
                .map(|index| (file_index, true, index))
                .or_else(|| {
                    program
                        .vaults
                        .iter()
                        .position(|vault| matches_path(&vault.path))
                        .map(|index| (file_index, false, index))
                })
        })
}

fn new_setup_file(path: &Path, directory: &Path) -> PathBuf {
    use sha2::{Digest, Sha256};
    let name: String = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let digest = format!("{:x}", Sha256::digest(path.to_string_lossy().as_bytes()));
    directory.join(format!("{name}-{}.enzyme", &digest[..8]))
}

/// Preserve explicit workspace policies; fill unspecified temporal choices from
/// Jev alongside entity and profile decisions. Profile prose comes from the
/// runtime's canonical catalog.
fn structured_setup(
    path: &Path,
    config: EnzymeConfig,
    temporal_policies: &std::collections::HashMap<String, Learning>,
) -> Result<Program> {
    let mut vault = config.reading_spec.clone().unwrap_or_default();
    let previous = std::mem::take(&mut vault.readings);
    vault.profiles.clear();
    vault.path = path.to_string_lossy().into_owned();
    vault.exclusions = config.excluded_folders;
    vault.excluded_tags = config.excluded_tags;
    vault.excluded_links = config.excluded_links;
    vault.frontmatter_link_fields = config.frontmatter_link_fields;
    vault.targets = config.targets;
    vault.embedding_limit = config.max_embedding_files;
    vault.format = config.catalyst_format;
    vault.learning.budget = vault
        .learning
        .budget
        .or(config.trending.and_then(|t| t.min_top_catalysts));
    let mut program = Program::default();
    for item in config.catalyzed_entities {
        let choices: Vec<_> = match item {
            CatalyzedEntity::Simple(entity) => vec![(entity, None, false)],
            CatalyzedEntity::WithOptions(items) => items
                .into_iter()
                .map(|(entity, options)| (entity, options.profile, options.expandable))
                .collect(),
        };
        for (entity, profile, expandable) in choices {
            let profile = profile.unwrap_or_else(|| "auto".into());
            if profile != "auto" {
                enzyme_core::llm::get_profile(&profile)
                    .with_context(|| format!("unknown generated profile: {profile}"))?;
            }
            let mut learning = previous
                .iter()
                .find(|r| r.entity.eq_ignore_ascii_case(&entity))
                .map(|r| r.learning.clone())
                .unwrap_or_else(Learning::default);
            let effective = learning.over(&vault.learning);
            // Sample/favor form one policy. Never reinterpret an explicit user
            // sample-only choice by filling its default preference from Jev.
            if effective.sample.is_none() && effective.favor.is_none() {
                if let Some(proposed) = temporal_policies.get(&entity) {
                    learning.sample = proposed.sample.clone();
                    learning.favor = proposed.favor.clone();
                }
            }
            vault.readings.push(Reading {
                entity,
                profile,
                definition: None,
                learning,
                include_linked_pages: expandable,
                include_who_links: false,
                pattern: false,
            });
        }
    }
    enzyme_spec::retain_patterns(&previous, &mut vault.readings);
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    program.workspaces.push(Workspace {
        name,
        sources: vec![Source::Markdown(MarkdownSource {
            name: "notes".into(),
            path: vault.path.clone(),
        })],
        learning: vault.learning,
        readings: vault.readings,
        automatic: vault.automatic,
        format: vault.format,
        targets: vault.targets,
        exclusions: vault.exclusions,
        excluded_tags: vault.excluded_tags,
        excluded_links: vault.excluded_links,
        frontmatter_link_fields: vault.frontmatter_link_fields,
        embedding_limit: vault.embedding_limit,
        memories: vault.memories,
        retrieval: vault.retrieval,
        note_policies: vault.note_policies,
    });
    Ok(program)
}

/// Theme-owned ANSI colors for keywords, strings, names, and numbers.
fn highlight(source: &str, enabled: bool) -> String {
    // Never let source text inject terminal control sequences, even without color.
    let source: String = source
        .chars()
        .flat_map(|c| {
            if c.is_control() && c != '\n' && c != '\t' {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect();
    if !enabled {
        return source;
    }
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let mut previous = String::new();
    while i < chars.len() {
        let start = i;
        let mut style = "";
        if chars[i] == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if chars[i] == '"' {
            i += 1;
            while i < chars.len() {
                if chars[i] == '\\' {
                    i = (i + 2).min(chars.len());
                } else if chars[i] == '"' {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
            style = "32";
        } else if chars[i].is_ascii_digit() {
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            style = "33";
        } else if chars[i].is_alphabetic() || chars[i] == '_' {
            while i < chars.len() && (chars[i].is_alphanumeric() || matches!(chars[i], '_' | '-')) {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            if matches!(word.as_str(), "question" | "questions") {
                style = "1;35";
            } else if matches!(
                word.as_str(),
                "folder" | "folders" | "tag" | "tags" | "link" | "links" | "log" | "logs"
            ) {
                style = "34";
            } else if matches!(
                word.as_str(),
                "learn"
                    | "project"
                    | "remember"
                    | "seek"
                    | "notice"
                    | "ask"
                    | "recognize"
                    | "retrieve"
                    | "answer"
                    | "leave"
                    | "prepare"
                    | "references"
                    | "produce"
                    | "select"
                    | "matching"
                    | "sample"
                    | "favor"
                    | "append"
            ) {
                style = "1;36";
            } else if matches!(
                word.as_str(),
                "vault" | "profile" | "learning" | "settings" | "let"
            ) {
                style = "36";
            } else if matches!(previous.as_str(), "profile" | "about" | "let") {
                style = "35";
            }
            previous = word;
        } else {
            i += 1;
        }
        let token: String = chars[start..i].iter().collect();
        if style.is_empty() {
            out.push_str(&token);
        } else {
            out.push_str(&format!("\x1b[{style}m{token}\x1b[0m"));
        }
    }
    out
}
