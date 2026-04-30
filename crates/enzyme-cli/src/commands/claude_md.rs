use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

const MARKER_START: &str = "<!-- enzyme:start -->";
const MARKER_END: &str = "<!-- enzyme:end -->";

const ENZYME_SECTION: &str = r#"<!-- enzyme:start -->
## Enzyme CLI

Use the Enzyme CLI for retrieving context from this vault. Run all enzyme commands from the current directory.

### Working memory

Enzyme petri provides your working memory — entities and catalysts (thematic phrases) from the vault.
If petri context was injected automatically (via hook), you already have it. If not, run it yourself
after the user's first message:

- **Specific prompt** ("what pages have I built", "remind me about the auth discussion"):
  `enzyme petri --query "user's question"` — returns entities and catalysts ranked by relevance.
- **Broad prompt** ("hey", "explore my ideas", "what have I been thinking about"):
  `enzyme petri` — returns the full landscape: top entities by recency with all their catalysts.

When petri includes nested children under a tag or folder, read them as evidence inside that parent cluster,
not as separate top-level themes by default.

Use the catalyst phrases as vocabulary for composing `enzyme catalyze` queries — they connect
to pre-computed content that the user's raw words won't find.

### Semantic search

`enzyme catalyze "query"` — search by concept/theme. Compose queries using catalyst vocabulary
from petri rather than the user's raw words.

Example flow:
1. Petri returns entity `design` with catalyst "iterative page design with distinct visual identities"
2. User asked "what pages have I built"
3. Compose: `enzyme catalyze "page creation iterative design visual identities"`

### Other commands

- `enzyme refresh` — re-index changed content (runs on hooks/crons, rarely needed manually)
- `enzyme apply ./target-dir` — index external content using vault catalysts, then `enzyme catalyze "query" --target ./target-dir`
- `grep` — exact tag, wikilink, or text matches (names, `#tags`, `[[links]]`)

**Tag search:** Tags appear as `- tag` in frontmatter and `#tag` inline. Search without `#` to catch both.

**Presentation registers:** Pass `--register` to catalyze to control framing:
- `explore` (default) — wonder, probe, notice patterns
- `continuity` — restore what the user knew, show trajectory, enable forward motion
- `reference` — surface what drew attention, connect imports to the user's own thinking

Results include `presentation_guidance` — follow these when framing the surfaced content.

### Presentation

Use Enzyme command names internally; do not expose `petri`, `catalyze`, catalyst IDs, scores, or tool names to the user unless asked.

Before making observations, ground them with `enzyme catalyze` excerpts. Lead with the user's words and file attribution, then add a small observation.

For broad exploration, use petri plus 1-2 catalyze searches, then open with one specific question about what the user is doing across their notes. Do not present a topic list.

For search results, do not lead with metadata. Notice tensions, repeated words, time gaps, or changes in framing across results. End with one concrete next direction, not a generic invitation.
<!-- enzyme:end -->"#;

/// Context files checked for existing Enzyme sections.
const CONTEXT_FILES: &[&str] = &[
    ".hermes.md",
    "HERMES.md",
    "AGENTS.md",
    "agents.md",
    "CLAUDE.md",
    "claude.md",
    ".cursorrules",
];

/// Find the best context file to write to:
/// 1. If any file already contains the enzyme marker, return it (update in place).
/// 2. Otherwise use AGENTS.md so Codex sees the instructions by default.
fn find_context_file(vault_path: &Path) -> std::path::PathBuf {
    // First pass: file that already has our marker
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
    // Default
    vault_path.join("AGENTS.md")
}

/// Claude Code reads CLAUDE.md, not AGENTS.md. When AGENTS.md is the canonical
/// instruction file, keep Claude wired to the same content through an import.
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

pub fn generate_claude_md(vault_path: &Path) -> Result<()> {
    let target_path = find_context_file(vault_path);

    let content = if target_path.exists() {
        fs::read_to_string(&target_path)
            .with_context(|| format!("failed to read {}", target_path.display()))?
    } else {
        String::new()
    };

    let new_content =
        if let (Some(start), Some(end)) = (content.find(MARKER_START), content.find(MARKER_END)) {
            // Replace existing section
            let end = end + MARKER_END.len();
            format!("{}{}{}", &content[..start], ENZYME_SECTION, &content[end..])
        } else if content.is_empty() {
            ENZYME_SECTION.to_string()
        } else {
            // Append to existing file
            format!("{}\n\n{}", content.trim_end(), ENZYME_SECTION)
        };

    fs::write(&target_path, new_content)
        .with_context(|| format!("failed to write {}", target_path.display()))?;

    ensure_claude_import(vault_path, &target_path)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn creates_agents_md_when_none_exists() {
        let dir = TempDir::new().unwrap();
        generate_claude_md(dir.path()).unwrap();

        let content = fs::read_to_string(dir.path().join("AGENTS.md")).unwrap();
        assert!(content.starts_with(MARKER_START));
        assert!(content.ends_with(MARKER_END));
        assert!(content.contains("enzyme petri"));

        let claude_content = fs::read_to_string(dir.path().join("CLAUDE.md")).unwrap();
        assert_eq!(claude_content, "@AGENTS.md\n");
    }

    #[test]
    fn writes_agents_and_imports_from_existing_claude_md() {
        let dir = TempDir::new().unwrap();
        let claude_md = dir.path().join("CLAUDE.md");
        fs::write(&claude_md, "# My Vault\n\nExisting instructions here.\n").unwrap();

        generate_claude_md(dir.path()).unwrap();

        let agents_content = fs::read_to_string(dir.path().join("AGENTS.md")).unwrap();
        assert!(agents_content.starts_with(MARKER_START));
        assert!(agents_content.contains("enzyme petri"));

        let claude_content = fs::read_to_string(&claude_md).unwrap();
        assert!(claude_content.starts_with("@AGENTS.md"));
        assert!(claude_content.contains("# My Vault"));
    }

    #[test]
    fn replaces_existing_enzyme_section() {
        let dir = TempDir::new().unwrap();
        let claude_md = dir.path().join("CLAUDE.md");
        let existing = format!(
            "# My Vault\n\n{}\nold content\n{}\n\n## Other Section\n",
            MARKER_START, MARKER_END
        );
        fs::write(&claude_md, &existing).unwrap();

        generate_claude_md(dir.path()).unwrap();

        let content = fs::read_to_string(&claude_md).unwrap();
        assert!(content.starts_with("# My Vault"));
        assert!(content.contains("enzyme petri"));
        assert!(!content.contains("old content"));
        assert!(content.contains("## Other Section"));
        // Only one start marker
        assert_eq!(content.matches(MARKER_START).count(), 1);
    }

    #[test]
    fn prefers_agents_md_over_claude_md() {
        let dir = TempDir::new().unwrap();
        let agents_md = dir.path().join("AGENTS.md");
        fs::write(&agents_md, "# My Agent\n\nSome instructions.\n").unwrap();
        // Also create CLAUDE.md — should not be touched
        let claude_md = dir.path().join("CLAUDE.md");
        fs::write(&claude_md, "# Claude\n").unwrap();

        generate_claude_md(dir.path()).unwrap();

        let agents_content = fs::read_to_string(&agents_md).unwrap();
        assert!(agents_content.contains(MARKER_START));
        assert!(agents_content.contains("enzyme petri"));
        // CLAUDE.md should import AGENTS.md but keep its existing content.
        let claude_content = fs::read_to_string(&claude_md).unwrap();
        assert!(claude_content.starts_with("@AGENTS.md"));
        assert!(claude_content.contains("# Claude"));
    }

    #[test]
    fn idempotent_on_repeated_runs() {
        let dir = TempDir::new().unwrap();

        generate_claude_md(dir.path()).unwrap();
        let first = fs::read_to_string(dir.path().join("AGENTS.md")).unwrap();
        let first_claude = fs::read_to_string(dir.path().join("CLAUDE.md")).unwrap();

        generate_claude_md(dir.path()).unwrap();
        let second = fs::read_to_string(dir.path().join("AGENTS.md")).unwrap();
        let second_claude = fs::read_to_string(dir.path().join("CLAUDE.md")).unwrap();

        assert_eq!(first, second);
        assert_eq!(first_claude, second_claude);
    }
}
