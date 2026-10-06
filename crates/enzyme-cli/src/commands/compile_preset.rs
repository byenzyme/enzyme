use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use enzyme_core::config::{EnzymePaths, SqliteSourceConfig};
use enzyme_core::sqlite_source::validate_sqlite_source;
use enzyme_spec::{Program, Source};

use super::arena;

const APPLE_NOTES: &str = include_str!("../../presets/apple-notes.enzyme.in");
const APPLE_NOTES_DATABASE: &str = include_str!("../../presets/apple-notes-database.enzyme.in");

/// `--preset <name|path>`: a built-in preset, or a `.enzyme.in` template file.
#[derive(Debug, Clone)]
pub enum Preset {
    Arena,
    AppleNotes,
    Template(PathBuf),
}

impl Preset {
    pub fn parse(raw: &str) -> Result<Self> {
        Ok(match raw {
            "arena" => Self::Arena,
            "apple-notes" => Self::AppleNotes,
            _ => {
                let path = PathBuf::from(raw);
                ensure!(
                    path.is_file(),
                    "unknown preset {raw:?}; use arena, apple-notes, or the path to a .enzyme.in template"
                );
                Self::Template(path)
            }
        })
    }

    fn label(&self) -> String {
        match self {
            Self::Arena => "arena".into(),
            Self::AppleNotes => "apple-notes".into(),
            Self::Template(path) => path.display().to_string(),
        }
    }
}

/// Fill a `.enzyme.in` template: `__WORKSPACE__` becomes the quoted workspace
/// name and `__SOURCE__` the quoted source argument. A template that uses
/// `__SOURCE__` needs exactly one source argument; one that does not takes none.
pub fn fill_template(template: &str, workspace_name: &str, sources: &[String]) -> Result<String> {
    let filled = template.replace("__WORKSPACE__", &serde_json::to_string(workspace_name)?);
    if template.contains("__SOURCE__") {
        ensure!(
            sources.len() == 1,
            "this preset needs exactly one source argument"
        );
        Ok(filled.replace("__SOURCE__", &serde_json::to_string(&sources[0])?))
    } else {
        ensure!(sources.is_empty(), "this preset takes no source arguments");
        Ok(filled)
    }
}

pub async fn run(
    preset: &str,
    sources: &[String],
    workspace_name: &str,
    output: Option<&Path>,
    dry_run: bool,
    json: bool,
    arena_api_base: &str,
) -> Result<()> {
    let preset = Preset::parse(preset)?;
    EnzymePaths::workspace_root(workspace_name)?;
    let home = EnzymePaths::enzyme_home()?;
    let directory = home.join("configs");
    let environment = enzyme_core::config::spec_environment(&user_home()?, &home)?;
    let (program, source, inspection) = match &preset {
        Preset::Arena => {
            let (program, channels) =
                arena::compile_preset_program(workspace_name, sources, arena_api_base).await?;
            let source = enzyme_spec::render_program(&program);
            (program, source, serde_json::json!({"channels": channels}))
        }
        Preset::AppleNotes => {
            ensure!(
                sources.len() <= 1,
                "the Apple Notes preset takes at most one database path"
            );
            let (template, arguments) = match sources.first() {
                Some(raw) => (
                    APPLE_NOTES_DATABASE,
                    vec![source_path(raw)?.to_string_lossy().into_owned()],
                ),
                None => (APPLE_NOTES, vec![]),
            };
            let source = fill_template(template, workspace_name, &arguments)?;
            let program = enzyme_spec::parse(&source).context("parse Apple Notes preset")?;
            let resolved = enzyme_spec::resolve_in(vec![program.clone()], &environment)?;
            let config = sqlite_source_config(&resolved, workspace_name)?;
            ensure!(
                config.db.is_file(),
                "Apple Notes database not found at {}; pass the path to NoteStore.sqlite",
                config.db.display()
            );
            let validation = validate_sqlite_source("notes", &config)
                .context("Apple Notes layout does not match this preset")?;
            ensure!(
                validation.row_count > 0,
                "Apple Notes preset found no active notes; inspect the database and edit the program"
            );
            (
                program,
                source,
                serde_json::json!({"database": config.db, "rows": validation.row_count}),
            )
        }
        Preset::Template(path) => {
            let template = std::fs::read_to_string(path)
                .with_context(|| format!("read preset {}", path.display()))?;
            let source = fill_template(&template, workspace_name, sources)?;
            let program = enzyme_spec::parse(&source)
                .with_context(|| format!("parse preset {}", path.display()))?;
            ensure!(
                program
                    .workspaces
                    .iter()
                    .any(|workspace| workspace.name == workspace_name),
                "preset {} declares no workspace __WORKSPACE__",
                path.display()
            );
            (program, source, serde_json::json!({"template": path}))
        }
    };
    let file_name = match preset {
        Preset::Arena => format!("arena-{}.enzyme", safe_file_name(workspace_name)),
        Preset::AppleNotes | Preset::Template(_) => {
            format!("{}.enzyme", safe_file_name(workspace_name))
        }
    };
    let destination = output
        .map(Path::to_path_buf)
        .unwrap_or_else(|| directory.join(file_name));
    validate_namespace(&directory, &destination, &program, &environment)?;

    let saved_to = if dry_run {
        None
    } else {
        save_without_overwrite(&destination, &source)?;
        Some(destination.as_path())
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "schema_version": "compile.v2",
                "preset": preset.label(),
                "workspace": workspace_name,
                "program": source,
                "saved_to": saved_to,
                "inspection": inspection,
            }))?
        );
    } else if dry_run {
        print!("{source}");
    } else {
        eprintln!("Saved program: {}", destination.display());
    }
    Ok(())
}

fn source_path(raw: &str) -> Result<PathBuf> {
    let path = if raw == "~" {
        user_home()?
    } else if let Some(rest) = raw.strip_prefix("~/") {
        user_home()?.join(rest)
    } else {
        PathBuf::from(raw)
    };
    ensure!(
        path.is_absolute(),
        "database path must be absolute or start with ~/"
    );
    let path = path
        .canonicalize()
        .with_context(|| format!("resolve database path {}", path.display()))?;
    ensure!(path.is_file(), "database is not a file: {}", path.display());
    Ok(path)
}

fn sqlite_source_config(program: &Program, workspace_name: &str) -> Result<SqliteSourceConfig> {
    let workspace = program
        .workspaces
        .iter()
        .find(|workspace| workspace.name == workspace_name)
        .context("preset has no requested workspace")?;
    ensure!(
        workspace
            .sources
            .iter()
            .any(|source| matches!(source, Source::Sqlite(source) if source.name == "notes")),
        "Apple Notes preset has no notes source"
    );
    let value = enzyme_spec::config_value(program);
    serde_json::from_value(value["workspaces"][workspace_name]["sources"]["notes"].clone())
        .context("compile Apple Notes source mapping")
}

fn validate_namespace(
    directory: &Path,
    destination: &Path,
    candidate: &Program,
    environment: &enzyme_spec::Environment,
) -> Result<()> {
    let mut programs = enzyme_spec::source_files(directory)?
        .into_iter()
        .filter(|(path, _)| path != destination)
        .map(|(_, program)| program)
        .collect::<Vec<_>>();
    programs.push(candidate.clone());
    enzyme_spec::resolve_in(programs, environment)
        .context("generated program conflicts with existing Enzyme configuration")?;
    Ok(())
}

fn save_without_overwrite(path: &Path, source: &str) -> Result<()> {
    if path.exists() {
        if std::fs::read_to_string(path)? == source {
            return Ok(());
        }
        bail!(
            "destination already exists: {}; use --dry-run to review and edit the existing program",
            path.display()
        );
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(source.as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary
        .persist_noclobber(path)
        .context("save new program")?;
    Ok(())
}

fn safe_file_name(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

fn user_home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME missing")
}
