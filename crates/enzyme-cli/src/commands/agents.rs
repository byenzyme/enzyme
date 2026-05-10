use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::ValueEnum;

const MARKER_START: &str = "<!-- enzyme:start -->";
const MARKER_END: &str = "<!-- enzyme:end -->";

const INSTRUCTIONS_URL: &str =
    "https://raw.githubusercontent.com/jshph/enzyme/main/agent-instructions.md";
const SKILL_URL: &str =
    "https://raw.githubusercontent.com/jshph/enzyme/main/agent-runtime-skill.md";

const LOCAL_INSTRUCTIONS: &str = include_str!("../../../../plugin/agent-instructions.md");
const LOCAL_SKILL: &str = include_str!("../../../../plugin/agent-runtime-skill.md");

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum AgentTarget {
    Codex,
    Claude,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum InstallSource {
    Auto,
    Remote,
    Local,
}

#[derive(Debug)]
struct LoadedTemplate {
    content: String,
    source: &'static str,
}

pub async fn run_agents_install(
    vault_path: &Path,
    target: AgentTarget,
    source: InstallSource,
) -> Result<()> {
    let instructions = load_template(INSTRUCTIONS_URL, LOCAL_INSTRUCTIONS, source).await?;
    let skill = load_template(SKILL_URL, LOCAL_SKILL, source).await?;

    let agents_path = write_agents_section(vault_path, &instructions.content)?;
    if matches!(target, AgentTarget::Claude) {
        ensure_claude_import(vault_path, &agents_path)?;
    }

    let mut installed = Vec::new();
    if matches!(target, AgentTarget::Codex) {
        installed.push(write_skill(vault_path, ".agents", &skill.content)?);
    }
    if matches!(target, AgentTarget::Claude) {
        installed.push(write_skill(vault_path, ".claude", &skill.content)?);
    }

    eprintln!(
        "Updated {} from {} copy.",
        agents_path.display(),
        instructions.source
    );
    for path in installed {
        eprintln!(
            "Installed skill {} from {} copy.",
            path.display(),
            skill.source
        );
    }

    Ok(())
}

async fn load_template(
    url: &'static str,
    local: &'static str,
    source: InstallSource,
) -> Result<LoadedTemplate> {
    match source {
        InstallSource::Local => Ok(LoadedTemplate {
            content: local.trim_end().to_string(),
            source: "packaged",
        }),
        InstallSource::Remote => Ok(LoadedTemplate {
            content: fetch_template(url).await?,
            source: "remote",
        }),
        InstallSource::Auto => match fetch_template(url).await {
            Ok(content) => Ok(LoadedTemplate {
                content,
                source: "remote",
            }),
            Err(e) => {
                tracing::warn!("failed to fetch {url}: {e}; using packaged copy");
                Ok(LoadedTemplate {
                    content: local.trim_end().to_string(),
                    source: "packaged",
                })
            }
        },
    }
}

async fn fetch_template(url: &str) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()?;
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("failed to fetch {url}"))?
        .error_for_status()
        .with_context(|| format!("failed to fetch {url}"))?;
    Ok(response.text().await?.trim_end().to_string())
}

fn write_agents_section(vault_path: &Path, section: &str) -> Result<PathBuf> {
    let path = find_context_file(vault_path);
    let content = if path.exists() {
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?
    } else {
        String::new()
    };

    let section = ensure_marked_section(section);
    let new_content =
        if let (Some(start), Some(end)) = (content.find(MARKER_START), content.find(MARKER_END)) {
            let end = end + MARKER_END.len();
            format!("{}{}{}", &content[..start], section, &content[end..])
        } else if content.is_empty() {
            format!("{section}\n")
        } else {
            format!("{}\n\n{section}\n", content.trim_end())
        };

    fs::write(&path, new_content).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

fn ensure_marked_section(section: &str) -> String {
    let trimmed = section.trim();
    if trimmed.contains(MARKER_START) && trimmed.contains(MARKER_END) {
        trimmed.to_string()
    } else {
        format!("{MARKER_START}\n{trimmed}\n{MARKER_END}")
    }
}

fn write_skill(vault_path: &Path, agent_dir: &str, skill: &str) -> Result<PathBuf> {
    let path = vault_path.join(agent_dir).join("skills").join("enzyme");
    fs::create_dir_all(&path).with_context(|| format!("failed to create {}", path.display()))?;
    let skill_path = path.join("SKILL.md");
    fs::write(&skill_path, format!("{}\n", skill.trim_end()))
        .with_context(|| format!("failed to write {}", skill_path.display()))?;
    Ok(skill_path)
}

const CONTEXT_FILES: &[&str] = &[
    ".hermes.md",
    "HERMES.md",
    "AGENTS.md",
    "agents.md",
    "CLAUDE.md",
    "claude.md",
    ".cursorrules",
];

fn find_context_file(vault_path: &Path) -> PathBuf {
    for name in CONTEXT_FILES {
        let path = vault_path.join(name);
        if path.exists() {
            if let Ok(content) = fs::read_to_string(&path) {
                if content.contains(MARKER_START) {
                    return path;
                }
            }
        }
    }
    vault_path.join("AGENTS.md")
}

fn ensure_claude_import(vault_path: &Path, target_path: &Path) -> Result<()> {
    let Some(file_name) = target_path.file_name().and_then(|name| name.to_str()) else {
        return Ok(());
    };
    if file_name != "AGENTS.md" && file_name != "agents.md" {
        return Ok(());
    }

    let claude_path = vault_path.join("CLAUDE.md");
    let import_line = format!("@{file_name}");

    if claude_path.exists() {
        let content = fs::read_to_string(&claude_path)
            .with_context(|| format!("failed to read {}", claude_path.display()))?;
        if content.lines().any(|line| line.trim() == import_line) {
            return Ok(());
        }
        let new_content = if content.trim().is_empty() {
            format!("{import_line}\n")
        } else {
            format!("{import_line}\n\n{}", content.trim_start())
        };
        fs::write(&claude_path, new_content)
            .with_context(|| format!("failed to write {}", claude_path.display()))?;
    } else {
        fs::write(&claude_path, format!("{import_line}\n"))
            .with_context(|| format!("failed to write {}", claude_path.display()))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn updates_marked_agents_section() {
        let dir = TempDir::new().unwrap();
        let agents = dir.path().join("AGENTS.md");
        fs::write(
            &agents,
            "# Notes\n\n<!-- enzyme:start -->\nold\n<!-- enzyme:end -->\n\n## Rest\n",
        )
        .unwrap();

        write_agents_section(dir.path(), LOCAL_INSTRUCTIONS).unwrap();

        let content = fs::read_to_string(agents).unwrap();
        assert!(content.contains("## Enzyme CLI"));
        assert!(!content.contains("\nold\n"));
        assert!(content.contains("## Rest"));
        assert_eq!(content.matches(MARKER_START).count(), 1);
    }

    #[test]
    fn installs_codex_skill() {
        let dir = TempDir::new().unwrap();
        let path = write_skill(dir.path(), ".agents", LOCAL_SKILL).unwrap();
        assert_eq!(path, dir.path().join(".agents/skills/enzyme/SKILL.md"));
        let content = fs::read_to_string(path).unwrap();
        assert!(content.contains("name: enzyme"));
    }
}
