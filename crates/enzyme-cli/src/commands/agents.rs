use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{env, fs};

use anyhow::{Context, Result};
use clap::ValueEnum;

const MARKER_START: &str = "<!-- enzyme:start -->";
const MARKER_END: &str = "<!-- enzyme:end -->";

const INSTRUCTIONS_URL: &str =
    "https://raw.githubusercontent.com/useenzyme/enzyme/main/agent-instructions.md";
const SKILL_URL: &str =
    "https://raw.githubusercontent.com/useenzyme/enzyme/main/agent-runtime-skill.md";
const WORKSPACE_SETUP_SKILL_URL: &str = "https://raw.githubusercontent.com/useenzyme/enzyme/main/skills/enzyme-workspace-setup/SKILL.md";

const LOCAL_INSTRUCTIONS: &str = include_str!("../../../../plugin/agent-instructions.md");
const LOCAL_SKILL: &str = include_str!("../../../../plugin/agent-runtime-skill.md");
const LOCAL_WORKSPACE_SETUP_SKILL: &str =
    include_str!("../../../../plugin/skills/enzyme-workspace-setup/SKILL.md");

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum AgentTarget {
    Codex,
    Claude,
    Hermes,
    Openclaw,
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

#[derive(Debug)]
struct InstalledSkill {
    path: PathBuf,
    source: &'static str,
}

pub async fn run_agents_install(
    vault_path: &Path,
    target: AgentTarget,
    source: InstallSource,
) -> Result<()> {
    let instructions = load_template(INSTRUCTIONS_URL, LOCAL_INSTRUCTIONS, source).await?;
    let skill = load_template(SKILL_URL, LOCAL_SKILL, source).await?;
    let workspace_setup_skill = if matches!(target, AgentTarget::Codex | AgentTarget::Claude) {
        Some(
            load_template(
                WORKSPACE_SETUP_SKILL_URL,
                LOCAL_WORKSPACE_SETUP_SKILL,
                source,
            )
            .await?,
        )
    } else {
        None
    };

    let agents_path = write_agents_section(vault_path, target, &instructions.content)?;
    if matches!(target, AgentTarget::Claude) {
        ensure_claude_import(vault_path, &agents_path)?;
    }

    let mut installed = Vec::new();
    if matches!(target, AgentTarget::Codex) {
        installed.push(InstalledSkill {
            path: write_named_skill(vault_path, ".agents", "enzyme", &skill.content)?,
            source: skill.source,
        });
        installed.push(InstalledSkill {
            path: write_named_skill(
                vault_path,
                ".agents",
                "enzyme-workspace-setup",
                &workspace_setup_skill
                    .as_ref()
                    .expect("workspace setup skill loaded for codex target")
                    .content,
            )?,
            source: workspace_setup_skill
                .as_ref()
                .expect("workspace setup skill loaded for codex target")
                .source,
        });
    }
    if matches!(target, AgentTarget::Claude) {
        installed.push(InstalledSkill {
            path: write_named_skill(vault_path, ".claude", "enzyme", &skill.content)?,
            source: skill.source,
        });
        installed.push(InstalledSkill {
            path: write_named_skill(
                vault_path,
                ".claude",
                "enzyme-workspace-setup",
                &workspace_setup_skill
                    .as_ref()
                    .expect("workspace setup skill loaded for claude target")
                    .content,
            )?,
            source: workspace_setup_skill
                .as_ref()
                .expect("workspace setup skill loaded for claude target")
                .source,
        });
    }
    if matches!(target, AgentTarget::Hermes) {
        ensure_hermes_instructions(vault_path, &agents_path)?;
        installed.push(InstalledSkill {
            path: write_hermes_skill(&skill.content)?,
            source: skill.source,
        });
    }
    if matches!(target, AgentTarget::Openclaw) {
        installed.push(InstalledSkill {
            path: write_openclaw_skill(&skill.content)?,
            source: skill.source,
        });
    }

    eprintln!(
        "Updated {} from {} copy.",
        agents_path.display(),
        instructions.source
    );
    for installed_skill in installed {
        eprintln!(
            "Installed skill {} from {} copy.",
            installed_skill.path.display(),
            installed_skill.source
        );
    }
    if matches!(target, AgentTarget::Codex) {
        eprintln!(
            "Next: read {} before running Enzyme setup commands.",
            vault_path
                .join(".agents/skills/enzyme-workspace-setup/SKILL.md")
                .display()
        );
    }
    if matches!(target, AgentTarget::Claude) {
        eprintln!(
            "Next: read {} before running Enzyme setup commands.",
            vault_path
                .join(".claude/skills/enzyme-workspace-setup/SKILL.md")
                .display()
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

fn write_agents_section(vault_path: &Path, target: AgentTarget, section: &str) -> Result<PathBuf> {
    let path = find_context_file_for_target(vault_path, target);
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

fn remove_marked_section(content: &str) -> String {
    if let (Some(start), Some(end)) = (content.find(MARKER_START), content.find(MARKER_END)) {
        let end = end + MARKER_END.len();
        let mut new_content = format!("{}{}", &content[..start], &content[end..]);
        while new_content.contains("\n\n\n") {
            new_content = new_content.replace("\n\n\n", "\n\n");
        }
        if new_content.trim().is_empty() {
            String::new()
        } else {
            format!("{}\n", new_content.trim())
        }
    } else {
        content.to_string()
    }
}

fn write_named_skill(
    vault_path: &Path,
    agent_dir: &str,
    skill_name: &str,
    skill: &str,
) -> Result<PathBuf> {
    let path = vault_path.join(agent_dir).join("skills").join(skill_name);
    fs::create_dir_all(&path).with_context(|| format!("failed to create {}", path.display()))?;
    let skill_path = path.join("SKILL.md");
    fs::write(&skill_path, format!("{}\n", skill.trim_end()))
        .with_context(|| format!("failed to write {}", skill_path.display()))?;
    Ok(skill_path)
}

fn write_hermes_skill(skill: &str) -> Result<PathBuf> {
    let hermes_home = hermes_home_dir();
    let path = hermes_home.join("skills").join("enzyme");
    fs::create_dir_all(&path).with_context(|| format!("failed to create {}", path.display()))?;
    let skill_path = path.join("SKILL.md");
    fs::write(&skill_path, format!("{}\n", skill.trim_end()))
        .with_context(|| format!("failed to write {}", skill_path.display()))?;
    Ok(skill_path)
}

fn write_openclaw_skill(skill: &str) -> Result<PathBuf> {
    let openclaw_home = openclaw_home_dir();
    let path = openclaw_home.join("skills").join("enzyme");
    fs::create_dir_all(&path).with_context(|| format!("failed to create {}", path.display()))?;
    let skill_path = path.join("SKILL.md");
    fs::write(&skill_path, format!("{}\n", skill.trim_end()))
        .with_context(|| format!("failed to write {}", skill_path.display()))?;
    Ok(skill_path)
}

fn hermes_home_dir() -> PathBuf {
    if let Some(home) = env::var_os("HERMES_HOME") {
        return PathBuf::from(home);
    }
    if let Some(home) = env::var_os("HOME") {
        return PathBuf::from(home).join(".hermes");
    }
    PathBuf::from(".hermes")
}

fn openclaw_home_dir() -> PathBuf {
    if let Some(home) = env::var_os("OPENCLAW_HOME") {
        return PathBuf::from(home);
    }
    if let Some(home) = env::var_os("HOME") {
        return PathBuf::from(home).join(".openclaw");
    }
    PathBuf::from(".openclaw")
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

fn find_context_file_for_target(vault_path: &Path, target: AgentTarget) -> PathBuf {
    if matches!(target, AgentTarget::Codex | AgentTarget::Claude) {
        return vault_path.join("AGENTS.md");
    }

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
        let content_without_legacy_section = remove_marked_section(&content);
        let has_import = content_without_legacy_section
            .lines()
            .any(|line| line.trim() == import_line);
        if has_import && content_without_legacy_section == content {
            return Ok(());
        }
        let new_content = if has_import {
            content_without_legacy_section
        } else if content_without_legacy_section.trim().is_empty() {
            format!("{import_line}\n")
        } else {
            format!(
                "{import_line}\n\n{}",
                content_without_legacy_section.trim_start()
            )
        };
        fs::write(&claude_path, new_content)
            .with_context(|| format!("failed to write {}", claude_path.display()))?;
    } else {
        fs::write(&claude_path, format!("{import_line}\n"))
            .with_context(|| format!("failed to write {}", claude_path.display()))?;
    }

    Ok(())
}

fn ensure_hermes_instructions(vault_path: &Path, target_path: &Path) -> Result<()> {
    let Some(file_name) = target_path.file_name().and_then(|name| name.to_str()) else {
        return Ok(());
    };

    let hermes_path = vault_path.join(".hermes.md");
    if hermes_path.exists() {
        return Ok(());
    }

    if file_name == "AGENTS.md" || file_name == "agents.md" {
        return Ok(());
    }

    fs::write(
        &hermes_path,
        format!(
            "# Hermes workspace context\n\nFollow the Enzyme instructions in `{}` when retrieving workspace context.\n",
            target_path.display()
        ),
    )
    .with_context(|| format!("failed to write {}", hermes_path.display()))?;

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

        write_agents_section(dir.path(), AgentTarget::Codex, LOCAL_INSTRUCTIONS).unwrap();

        let content = fs::read_to_string(agents).unwrap();
        assert!(content.contains("## Enzyme Workspace Context"));
        assert!(content.contains("enzyme-workspace-setup/SKILL.md"));
        assert!(!content.contains("scan --write-config"));
        assert!(!content.contains("Retrieval defaults"));
        assert!(!content.contains("Presentation policy"));
        assert!(!content.contains("Structure policy"));
        assert!(!content.contains("\nold\n"));
        assert!(content.contains("## Rest"));
        assert_eq!(content.matches(MARKER_START).count(), 1);
    }

    #[test]
    fn codex_target_writes_agents_even_when_claude_has_marker() {
        let dir = TempDir::new().unwrap();
        let claude = dir.path().join("CLAUDE.md");
        fs::write(
            &claude,
            "# Claude\n\n<!-- enzyme:start -->\nold\n<!-- enzyme:end -->\n",
        )
        .unwrap();

        let agents =
            write_agents_section(dir.path(), AgentTarget::Codex, LOCAL_INSTRUCTIONS).unwrap();

        assert_eq!(agents, dir.path().join("AGENTS.md"));
        assert!(
            fs::read_to_string(&agents)
                .unwrap()
                .contains("## Enzyme Workspace Context")
        );
        assert!(fs::read_to_string(&claude).unwrap().contains("\nold\n"));
    }

    #[test]
    fn claude_target_imports_agents_and_removes_legacy_claude_marker() {
        let dir = TempDir::new().unwrap();
        let claude = dir.path().join("CLAUDE.md");
        fs::write(
            &claude,
            "# Claude notes\n\n<!-- enzyme:start -->\nold\n<!-- enzyme:end -->\n\nKeep this.\n",
        )
        .unwrap();

        let agents =
            write_agents_section(dir.path(), AgentTarget::Claude, LOCAL_INSTRUCTIONS).unwrap();
        ensure_claude_import(dir.path(), &agents).unwrap();

        assert_eq!(agents, dir.path().join("AGENTS.md"));
        let claude_content = fs::read_to_string(&claude).unwrap();
        assert!(claude_content.starts_with("@AGENTS.md\n\n"));
        assert!(claude_content.contains("# Claude notes"));
        assert!(claude_content.contains("Keep this."));
        assert!(!claude_content.contains(MARKER_START));
        assert!(!claude_content.contains("\nold\n"));
    }

    #[test]
    fn installs_codex_skill() {
        let dir = TempDir::new().unwrap();
        let path = write_named_skill(dir.path(), ".agents", "enzyme", LOCAL_SKILL).unwrap();
        assert_eq!(path, dir.path().join(".agents/skills/enzyme/SKILL.md"));
        let content = fs::read_to_string(path).unwrap();
        assert!(content.contains("name: enzyme"));

        let setup_path = write_named_skill(
            dir.path(),
            ".agents",
            "enzyme-workspace-setup",
            LOCAL_WORKSPACE_SETUP_SKILL,
        )
        .unwrap();
        assert_eq!(
            setup_path,
            dir.path()
                .join(".agents/skills/enzyme-workspace-setup/SKILL.md")
        );
        let setup_content = fs::read_to_string(setup_path).unwrap();
        assert!(setup_content.contains("name: enzyme-workspace-setup"));
    }

    #[test]
    fn hermes_target_uses_agents_when_available() {
        let dir = TempDir::new().unwrap();
        let agents =
            write_agents_section(dir.path(), AgentTarget::Hermes, LOCAL_INSTRUCTIONS).unwrap();

        ensure_hermes_instructions(dir.path(), &agents).unwrap();

        assert_eq!(agents, dir.path().join("AGENTS.md"));
        assert!(!dir.path().join(".hermes.md").exists());
    }

    #[test]
    fn hermes_home_defaults_to_env_override() {
        let dir = TempDir::new().unwrap();
        unsafe {
            env::set_var("HERMES_HOME", dir.path());
        }
        let path = write_hermes_skill(LOCAL_SKILL).unwrap();
        unsafe {
            env::remove_var("HERMES_HOME");
        }

        assert_eq!(path, dir.path().join("skills/enzyme/SKILL.md"));
        let content = fs::read_to_string(path).unwrap();
        assert!(content.contains("name: enzyme"));
    }

    #[test]
    fn openclaw_home_defaults_to_env_override() {
        let dir = TempDir::new().unwrap();
        unsafe {
            env::set_var("OPENCLAW_HOME", dir.path());
        }
        let path = write_openclaw_skill(LOCAL_SKILL).unwrap();
        unsafe {
            env::remove_var("OPENCLAW_HOME");
        }

        assert_eq!(path, dir.path().join("skills/enzyme/SKILL.md"));
        let content = fs::read_to_string(path).unwrap();
        assert!(content.contains("name: enzyme"));
    }
}
