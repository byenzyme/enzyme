use anyhow::{Context, Result, ensure};
use enzyme_core::config::{
    EnzymeConfig,
    enzyme_config::{CatalyzedEntity, readable_builtin_profiles},
};
use enzyme_spec::{Learning, Program, Reading};
use std::{
    io::{self, IsTerminal, Write},
    path::Path,
};

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

/// Generate structured choices first; rendering never invokes a model.
pub async fn run(path: &Path, color: ColorChoice, output: Option<&Path>) -> Result<()> {
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
    ensure!(
        config.sources.is_empty() && config.notes_path.is_none() && config.notes_roots.is_empty(),
        "compile supports a single Markdown vault; structured sources must keep their existing configuration"
    );
    let generated = super::scan::generate_jev_setup(&path).await?;
    ensure!(
        !generated.proposal.entities.is_empty(),
        "Jev selected no entities; no configuration generated"
    );
    for folder in generated.excluded_folders() {
        if !config.excluded_folders.contains(folder) {
            config.excluded_folders.push(folder.clone());
        }
    }
    config.catalyzed_entities = generated.proposal.entities;
    let program = structured_setup(&path, config)?;
    let source = enzyme_spec::render_program(&program);
    // Exercise the same parser/resolver as init before emitting any configuration.
    enzyme_spec::resolve(vec![enzyme_spec::parse(&source)?], &path)?;
    if let Some(file) = output {
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
        eprintln!("Saved proposal: {}", file.display());
    }
    let enabled = match color {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => {
            io::stdout().is_terminal()
                && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
                && std::env::var("TERM").as_deref() != Ok("dumb")
        }
    };
    io::stdout()
        .lock()
        .write_all(highlight(&source, enabled).as_bytes())?;
    eprintln!(
        "Generated {} readings in {} ms. Review the proposal before adding it to your Enzyme configs.",
        program.vaults[0].readings.len(),
        start.elapsed().as_millis()
    );
    Ok(())
}

/// Preserve deterministic workspace policies; only entities and profile choices
/// come from Jev. Profile prose comes from the runtime's canonical catalog.
fn structured_setup(path: &Path, config: EnzymeConfig) -> Result<Program> {
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
    let catalog = readable_builtin_profiles();
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
                let definition = catalog
                    .get(&profile)
                    .with_context(|| format!("unknown generated profile: {profile}"))?;
                program.profiles.insert(profile.clone(), definition.clone());
            }
            let learning = previous
                .iter()
                .find(|r| r.entity.eq_ignore_ascii_case(&entity))
                .map(|r| r.learning.clone())
                .unwrap_or_else(Learning::default);
            vault.readings.push(Reading {
                entity,
                profile,
                definition: None,
                learning,
                include_linked_pages: expandable,
            });
        }
    }
    program.vaults.push(vault);
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
