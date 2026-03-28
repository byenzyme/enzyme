use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

const MARKER_START: &str = "<!-- enzyme:start -->";
const MARKER_END: &str = "<!-- enzyme:end -->";

const ENZYME_SECTION: &str = r#"<!-- enzyme:start -->
## Enzyme CLI

Use the Enzyme CLI for retrieving context from this vault. Run all commands from the vault root.

### Working memory

Enzyme petri provides your working memory — entities and catalysts (thematic phrases) from the vault.
If petri context was injected automatically (via hook), you already have it. If not, run it yourself
after the user's first message:

- **Specific prompt** ("what pages have I built", "remind me about the auth discussion"):
  `enzyme petri --query "user's question"` — returns entities and catalysts ranked by relevance.
- **Broad prompt** ("hey", "explore my ideas", "what have I been thinking about"):
  `enzyme petri` — returns the full landscape: top entities by recency with all their catalysts.

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
<!-- enzyme:end -->"#;

pub fn generate_claude_md(vault_path: &Path) -> Result<()> {
    let claude_md_path = vault_path.join("CLAUDE.md");

    let content = if claude_md_path.exists() {
        fs::read_to_string(&claude_md_path)
            .context("failed to read existing CLAUDE.md")?
    } else {
        String::new()
    };

    let new_content = if let (Some(start), Some(end)) = (
        content.find(MARKER_START),
        content.find(MARKER_END),
    ) {
        // Replace existing section
        let end = end + MARKER_END.len();
        format!("{}{}{}", &content[..start], ENZYME_SECTION, &content[end..])
    } else if content.is_empty() {
        ENZYME_SECTION.to_string()
    } else {
        // Append to existing file
        format!("{}\n\n{}", content.trim_end(), ENZYME_SECTION)
    };

    fs::write(&claude_md_path, new_content)
        .context("failed to write CLAUDE.md")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn creates_claude_md_when_none_exists() {
        let dir = TempDir::new().unwrap();
        generate_claude_md(dir.path()).unwrap();

        let content = fs::read_to_string(dir.path().join("CLAUDE.md")).unwrap();
        assert!(content.starts_with(MARKER_START));
        assert!(content.ends_with(MARKER_END));
        assert!(content.contains("enzyme petri"));
    }

    #[test]
    fn appends_to_existing_claude_md() {
        let dir = TempDir::new().unwrap();
        let claude_md = dir.path().join("CLAUDE.md");
        fs::write(&claude_md, "# My Vault\n\nExisting instructions here.\n").unwrap();

        generate_claude_md(dir.path()).unwrap();

        let content = fs::read_to_string(&claude_md).unwrap();
        assert!(content.starts_with("# My Vault"));
        assert!(content.contains(MARKER_START));
        assert!(content.contains("enzyme petri"));
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
    fn idempotent_on_repeated_runs() {
        let dir = TempDir::new().unwrap();

        generate_claude_md(dir.path()).unwrap();
        let first = fs::read_to_string(dir.path().join("CLAUDE.md")).unwrap();

        generate_claude_md(dir.path()).unwrap();
        let second = fs::read_to_string(dir.path().join("CLAUDE.md")).unwrap();

        assert_eq!(first, second);
    }
}
