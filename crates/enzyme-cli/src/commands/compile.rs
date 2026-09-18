use anyhow::{Context, Result, ensure};
use enzyme_core::config::{EnzymeConfig, EnzymePaths, enzyme_config::CatalyzedEntity};
use enzyme_spec::{Learning, Program, Reading, Vault};
use sha2::{Digest, Sha256};
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

pub fn run(path: &Path, verbose: bool, color: ColorChoice) -> Result<()> {
    let path = path.canonicalize().context("resolve vault directory")?;
    ensure!(path.is_dir(), "vault must be a directory");
    let home = EnzymePaths::enzyme_home()?;
    let directory = home.join("configs");
    // Validate the complete namespace first. An invalid file must not be overwritten.
    let config = EnzymeConfig::load(&path)?;
    let user_home = std::env::var_os("HOME").context("HOME missing")?;
    for (file, program) in enzyme_spec::source_files(&directory)? {
        let matches = program.vaults.iter().any(|v| {
            let expanded = v
                .path
                .strip_prefix("~/")
                .map(|p| Path::new(&user_home).join(p))
                .unwrap_or_else(|| v.path.clone().into());
            expanded.canonicalize().unwrap_or(expanded) == path
        });
        if matches {
            return display(
                &file,
                &std::fs::read_to_string(&file)?,
                "Validated; existing setup preserved",
                verbose,
                color,
            );
        }
    }
    let mut config = config.unwrap_or_default();
    ensure!(
        config.sources.is_empty() && config.notes_path.is_none() && config.notes_roots.is_empty(),
        "compile supports a single Markdown vault; structured sources must keep their existing configuration"
    );
    if config.catalyzed_entities.is_empty() {
        let starter = super::scan::starter_config(&path)?;
        config.catalyzed_entities = starter.catalyzed_entities;
        for folder in starter.excluded_folders {
            if !config.excluded_folders.contains(&folder) {
                config.excluded_folders.push(folder);
            }
        }
    }
    let mut vault = Vault {
        path: path.to_string_lossy().into_owned(),
        exclusions: config.excluded_folders,
        excluded_tags: config.excluded_tags,
        excluded_links: config.excluded_links,
        frontmatter_link_fields: config.frontmatter_link_fields,
        targets: config.targets,
        embedding_limit: config.max_embedding_files,
        format: config.catalyst_format,
        learning: Learning {
            budget: config.trending.and_then(|t| t.min_top_catalysts),
            ..Default::default()
        },
        ..Default::default()
    };
    for item in config.catalyzed_entities {
        match item {
            CatalyzedEntity::Simple(entity) => vault.readings.push(Reading {
                entity,
                profile: "auto".into(),
                definition: None,
                learning: Learning::default(),
                include_linked_pages: false,
            }),
            CatalyzedEntity::WithOptions(items) => {
                for (entity, options) in items {
                    vault.readings.push(Reading {
                        entity,
                        profile: options.profile.unwrap_or_else(|| "auto".into()),
                        include_linked_pages: options.expandable,
                        definition: None,
                        learning: Learning::default(),
                    });
                }
            }
        }
    }
    let source = enzyme_spec::render_program(&Program {
        vaults: vec![vault],
        ..Default::default()
    });
    // Validate alongside reusable definitions and global settings before publishing.
    let mut namespace: Vec<_> = enzyme_spec::source_files(&directory)?
        .into_iter()
        .map(|(_, p)| p)
        .collect();
    namespace.push(enzyme_spec::parse(&source)?);
    enzyme_spec::resolve(namespace, Path::new(&user_home))?;
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
    let file = directory.join(format!("{name}-{}.enzyme", &digest[..8]));
    ensure!(
        !file.exists(),
        "destination already exists: {}",
        file.display()
    );
    enzyme_spec::atomic_write(&file, &source)?;
    display(&file, &source, "Saved readable setup", verbose, color)
}

fn display(
    file: &Path,
    source: &str,
    status: &str,
    verbose: bool,
    color: ColorChoice,
) -> Result<()> {
    if verbose {
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
            .write_all(highlight(source, enabled).as_bytes())?;
    }
    eprintln!("{status}: {}", file.display());
    eprintln!("Next: enzyme init (from this vault)");
    Ok(())
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
            if matches!(
                word.as_str(),
                "vault"
                    | "profile"
                    | "learn"
                    | "questions"
                    | "from"
                    | "folder"
                    | "folders"
                    | "tag"
                    | "tags"
                    | "link"
                    | "links"
                    | "log"
                    | "logs"
                    | "including"
                    | "linked"
                    | "pages"
                    | "about"
                    | "seek"
                    | "notice"
                    | "ask"
                    | "recognize"
                    | "note"
                    | "learning"
                    | "settings"
                    | "when"
                    | "asked"
                    | "retrieve"
                    | "passages"
                    | "across"
                    | "the"
                    | "through"
                    | "learned"
                    | "answer"
                    | "with"
                    | "sources"
                    | "leave"
                    | "out"
                    | "prepare"
                    | "up"
                    | "to"
                    | "documents"
                    | "per"
                    | "run"
                    | "newest"
                    | "first"
                    | "references"
                    | "in"
                    | "fields"
                    | "produce"
                    | "theses"
                    | "claims"
                    | "sample"
                    | "time"
                    | "favor"
                    | "recent"
                    | "periods"
                    | "all"
                    | "equally"
                    | "question"
                    | "budget"
                    | "project"
                    | "into"
                    | "remember"
                    | "append"
                    | "observation"
                    | "let"
                    | "generation"
                    | "auto"
                    | "local"
                    | "hosted"
                    | "model"
                    | "updates"
                    | "enabled"
                    | "disabled"
                    | "embedding"
                    | "limit"
                    | "format"
                    | "thesis"
                    | "claim"
                    | "selection"
            ) {
                style = "1;36";
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
