use std::collections::HashSet;
use std::path::Path;

/// Entities extracted from a markdown document's content and file path.
#[derive(Debug, Clone, Default)]
pub struct ExtractedEntities {
    /// Hashtags found in body text and frontmatter, normalized to lowercase.
    pub tags: Vec<String>,
    /// Wikilink targets (the part before `|` if aliased).
    pub links: Vec<String>,
    /// Folder path components derived from the file path.
    pub folders: Vec<String>,
}

/// Extract tags, wikilinks, and folder entities from markdown content.
///
/// Tags from `frontmatter_tags` are merged (deduplicated) with inline tags.
/// Inline tags inside fenced code blocks are ignored.
pub fn extract_entities(
    content: &str,
    file_path: &Path,
    frontmatter_tags: &[String],
) -> ExtractedEntities {
    let mut tag_set: HashSet<String> = HashSet::new();
    let mut link_set: HashSet<String> = HashSet::new();

    // -----------------------------------------------------------------------
    // Tags from frontmatter (already normalized by frontmatter parser)
    // -----------------------------------------------------------------------
    for t in frontmatter_tags {
        tag_set.insert(t.clone());
    }

    // -----------------------------------------------------------------------
    // Remove fenced code blocks before scanning for inline entities.
    // We replace code block regions with whitespace so character offsets are
    // preserved but no tags/links are extracted from inside code.
    // -----------------------------------------------------------------------
    let cleaned = remove_code_blocks(content);

    // -----------------------------------------------------------------------
    // Inline hashtags: #tag, #nested/tag
    // Must be preceded by whitespace or start of line.
    // Tag characters: alphanumeric, `-`, `_`, `/`
    // -----------------------------------------------------------------------
    extract_inline_tags(&cleaned, &mut tag_set);

    // -----------------------------------------------------------------------
    // Wikilinks: [[Target]] or [[Target|Display]]
    // -----------------------------------------------------------------------
    extract_wikilinks(&cleaned, &mut link_set);

    // -----------------------------------------------------------------------
    // Folders from file path
    // -----------------------------------------------------------------------
    let folders = extract_folders(file_path);

    // Filter out Readwise metadata tags (e.g., readwise/source/reader),
    // numeric-only tags (footnote numbers, heading numbers),
    // and hex color codes (#ff0000, #333, #1a2b3c)
    tag_set.retain(|t| !t.starts_with("readwise/") && !is_numeric_only(t) && !is_hex_color(t));

    // Expand hierarchical tags into parent components.
    // e.g., "type/book" → also indexes "type"
    //        "projects/rust/enzyme" → also indexes "projects" and "projects/rust"
    let expanded: Vec<String> = tag_set
        .iter()
        .filter(|t| t.contains('/'))
        .flat_map(|t| {
            let segments: Vec<&str> = t.split('/').collect();
            (1..segments.len()).map(move |i| segments[..i].join("/"))
        })
        .collect();
    for parent in expanded {
        tag_set.insert(parent);
    }

    let mut tags: Vec<String> = tag_set.into_iter().collect();
    tags.sort();

    // Filter out date-formatted links (YYYY-MM-DD) — Readwise highlight dates
    // and overly long links (broken extraction artifacts)
    link_set.retain(|l| !is_date_link(l) && l.len() <= 200);

    // Normalize links to lowercase for consistent matching
    let mut links: Vec<String> = link_set
        .into_iter()
        .map(|l| l.to_lowercase())
        .collect::<HashSet<String>>()
        .into_iter()
        .collect();
    links.sort();

    ExtractedEntities {
        tags,
        links,
        folders,
    }
}

/// Check if a string is purely numeric (possibly with leading zeros).
fn is_numeric_only(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// Check if a string looks like a CSS hex color code.
///
/// Matches 3, 4, 6, or 8 hex digit patterns (e.g., "fff", "ff0000", "1a2b3c4d").
fn is_hex_color(s: &str) -> bool {
    let len = s.len();
    (len == 3 || len == 4 || len == 6 || len == 8) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Check if a link target is a bare date (YYYY-MM-DD).
///
/// These are typically Readwise-generated highlight dates embedded as
/// `[[2022-10-23]]` and are not meaningful vault entities.
fn is_date_link(link: &str) -> bool {
    let bytes = link.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[..4].iter().all(|b| b.is_ascii_digit())
        && bytes[5..7].iter().all(|b| b.is_ascii_digit())
        && bytes[8..10].iter().all(|b| b.is_ascii_digit())
}

/// Remove fenced code blocks (``` ... ```) from the content, replacing
/// their interior with spaces so we can safely scan for tags and links.
fn remove_code_blocks(content: &str) -> String {
    let mut result = String::with_capacity(content.len());
    let mut in_code_block = false;
    let mut fence_char: Option<char> = None;
    let mut fence_len: usize = 0;

    for line in content.lines() {
        let trimmed = line.trim_start();
        if !in_code_block {
            // Check for opening fence
            if let Some((ch, len)) = detect_fence(trimmed) {
                in_code_block = true;
                fence_char = Some(ch);
                fence_len = len;
                // Replace the line content with spaces
                result.extend(std::iter::repeat_n(' ', line.len()));
                result.push('\n');
                continue;
            }
            result.push_str(line);
            result.push('\n');
        } else {
            // Check for closing fence
            if let Some((ch, len)) = detect_fence(trimmed) {
                if Some(ch) == fence_char && len >= fence_len {
                    in_code_block = false;
                    fence_char = None;
                    fence_len = 0;
                }
            }
            // Inside code block — replace with spaces
            result.extend(std::iter::repeat_n(' ', line.len()));
            result.push('\n');
        }
    }

    result
}

/// Detect a code fence at the start of a trimmed line.
/// Returns the fence character and its count if found.
fn detect_fence(trimmed_line: &str) -> Option<(char, usize)> {
    let first = trimmed_line.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let count = trimmed_line.chars().take_while(|&c| c == first).count();
    if count >= 3 {
        Some((first, count))
    } else {
        None
    }
}

/// Extract inline hashtags from content (code blocks already removed).
fn extract_inline_tags(content: &str, tags: &mut HashSet<String>) {
    let chars: Vec<char> = content.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        if chars[i] == '#' {
            // Must be preceded by whitespace, newline, or be at position 0
            let preceded_ok = i == 0 || chars[i - 1].is_whitespace();
            if preceded_ok {
                // Collect tag characters: alphanumeric, -, _, /
                let start = i + 1;
                let mut end = start;
                while end < len
                    && (chars[end].is_alphanumeric()
                        || chars[end] == '-'
                        || chars[end] == '_'
                        || chars[end] == '/')
                {
                    end += 1;
                }
                // Must have at least one character and not end with /
                if end > start {
                    let tag: String = chars[start..end].iter().collect();
                    let tag = tag.trim_end_matches('/');
                    if !tag.is_empty() {
                        tags.insert(tag.to_lowercase());
                    }
                }
                i = end;
                continue;
            }
        }
        i += 1;
    }
}

/// Extract wikilink targets from content (code blocks already removed).
fn extract_wikilinks(content: &str, links: &mut HashSet<String>) {
    let mut search_from = 0;
    let bytes = content.as_bytes();

    while search_from + 3 < bytes.len() {
        // Find opening `[[`
        let Some(open) = content[search_from..].find("[[") else {
            break;
        };
        let abs_open = search_from + open + 2; // position after `[[`

        // Find closing `]]`
        let Some(close) = content[abs_open..].find("]]") else {
            break;
        };
        let abs_close = abs_open + close;

        let inner = &content[abs_open..abs_close];

        // Ignore if it contains a newline (malformed)
        if !inner.contains('\n') && !inner.is_empty() {
            // Take the target (before `|` if aliased)
            let target = inner.split('|').next().unwrap_or(inner).trim();
            if !target.is_empty() {
                links.insert(target.to_string());
            }
        }

        search_from = abs_close + 2;
    }
}

/// Derive folder entities from file path components.
///
/// For `notes/daily/2024-01-15.md`, this returns:
/// `["notes", "notes/daily"]`
fn extract_folders(file_path: &Path) -> Vec<String> {
    let mut folders = Vec::new();
    let components: Vec<&str> = file_path
        .parent()
        .map(|p| {
            p.components()
                .map(|c| c.as_os_str().to_str().unwrap_or(""))
                .collect()
        })
        .unwrap_or_default();

    let mut prefix = String::new();
    for (i, component) in components.iter().enumerate() {
        if component.is_empty() {
            continue;
        }
        if i > 0 && !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(&component.to_lowercase());
        folders.push(prefix.clone());
    }

    folders
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_extract_inline_tags() {
        let content = "Some text #rust and #programming here";
        let entities = extract_entities(content, &PathBuf::from("note.md"), &[]);

        assert!(entities.tags.contains(&"rust".to_string()));
        assert!(entities.tags.contains(&"programming".to_string()));
    }

    #[test]
    fn test_extract_nested_tags() {
        let content = "Check #projects/rust/enzyme tag";
        let entities = extract_entities(content, &PathBuf::from("note.md"), &[]);

        assert!(entities.tags.contains(&"projects/rust/enzyme".to_string()));
        // Hierarchical expansion: parent components are also indexed
        assert!(entities.tags.contains(&"projects".to_string()));
        assert!(entities.tags.contains(&"projects/rust".to_string()));
    }

    #[test]
    fn test_hierarchical_tag_expansion_frontmatter() {
        let frontmatter_tags = vec!["type/book".to_string()];
        let entities = extract_entities("content", &PathBuf::from("note.md"), &frontmatter_tags);

        assert!(entities.tags.contains(&"type/book".to_string()));
        assert!(entities.tags.contains(&"type".to_string()));
    }

    #[test]
    fn test_hierarchical_expansion_no_duplicates() {
        // If parent tag already exists explicitly, no duplication
        let content = "#type #type/book";
        let entities = extract_entities(content, &PathBuf::from("note.md"), &[]);

        assert!(entities.tags.contains(&"type".to_string()));
        assert!(entities.tags.contains(&"type/book".to_string()));
        let type_count = entities.tags.iter().filter(|t| *t == "type").count();
        assert_eq!(type_count, 1);
    }

    #[test]
    fn test_tags_normalized_lowercase() {
        let content = "#Rust #PROGRAMMING #MixedCase";
        let entities = extract_entities(content, &PathBuf::from("note.md"), &[]);

        assert!(entities.tags.contains(&"rust".to_string()));
        assert!(entities.tags.contains(&"programming".to_string()));
        assert!(entities.tags.contains(&"mixedcase".to_string()));
    }

    #[test]
    fn test_tags_deduplicated_with_frontmatter() {
        let content = "Some #rust content";
        let frontmatter_tags = vec!["rust".to_string(), "extra".to_string()];
        let entities = extract_entities(content, &PathBuf::from("note.md"), &frontmatter_tags);

        // "rust" appears both in frontmatter and content, should not be duplicated
        let rust_count = entities.tags.iter().filter(|t| *t == "rust").count();
        assert_eq!(rust_count, 1);
        assert!(entities.tags.contains(&"extra".to_string()));
    }

    #[test]
    fn test_tags_not_in_code_blocks() {
        let content = "\
Some #valid-tag here

```rust
let x = #not_a_tag;
println!(\"#also_not_a_tag\");
```

Another #real-tag
";
        let entities = extract_entities(content, &PathBuf::from("note.md"), &[]);

        assert!(entities.tags.contains(&"valid-tag".to_string()));
        assert!(entities.tags.contains(&"real-tag".to_string()));
        assert!(!entities.tags.contains(&"not_a_tag".to_string()));
        assert!(!entities.tags.contains(&"also_not_a_tag".to_string()));
    }

    #[test]
    fn test_tags_not_in_tilde_code_blocks() {
        let content = "\
#before

~~~
#inside_tilde
~~~

#after
";
        let entities = extract_entities(content, &PathBuf::from("note.md"), &[]);

        assert!(entities.tags.contains(&"before".to_string()));
        assert!(entities.tags.contains(&"after".to_string()));
        assert!(!entities.tags.contains(&"inside_tilde".to_string()));
    }

    #[test]
    fn test_extract_wikilinks() {
        let content = "See [[My Note]] and [[Another Note|display text]] for details.";
        let entities = extract_entities(content, &PathBuf::from("note.md"), &[]);

        assert!(entities.links.contains(&"my note".to_string()));
        assert!(entities.links.contains(&"another note".to_string()));
        // Display text should not appear as a separate link
        assert!(!entities.links.contains(&"display text".to_string()));
    }

    #[test]
    fn test_extract_wikilinks_with_path() {
        let content = "Link to [[folder/subfolder/Note Name]]";
        let entities = extract_entities(content, &PathBuf::from("note.md"), &[]);

        assert!(
            entities
                .links
                .contains(&"folder/subfolder/note name".to_string())
        );
    }

    #[test]
    fn test_extract_folders() {
        let path = PathBuf::from("notes/daily/2024-01-15.md");
        let entities = extract_entities("content", &path, &[]);

        assert_eq!(entities.folders, vec!["notes", "notes/daily"]);
    }

    #[test]
    fn test_extract_folders_root_file() {
        let path = PathBuf::from("note.md");
        let entities = extract_entities("content", &path, &[]);

        assert!(entities.folders.is_empty());
    }

    #[test]
    fn test_extract_folders_deep_nesting() {
        let path = PathBuf::from("a/b/c/note.md");
        let entities = extract_entities("content", &path, &[]);

        assert_eq!(entities.folders, vec!["a", "a/b", "a/b/c"]);
    }

    #[test]
    fn test_hash_not_preceded_by_whitespace_ignored() {
        // In the middle of a word, # should not be a tag
        let content = "email@#company or C#sharp";
        let entities = extract_entities(content, &PathBuf::from("note.md"), &[]);

        // #company could match because @ is not alphanumeric but let's check
        // Actually `@` is not whitespace, so it should be ignored
        assert!(!entities.tags.contains(&"sharp".to_string()));
    }

    #[test]
    fn test_empty_wikilink_ignored() {
        let content = "Empty [[]] link";
        let entities = extract_entities(content, &PathBuf::from("note.md"), &[]);

        assert!(entities.links.is_empty());
    }

    #[test]
    fn test_tags_with_hyphens_and_underscores() {
        let content = "#my-tag #another_tag #mixed-tag_here";
        let entities = extract_entities(content, &PathBuf::from("note.md"), &[]);

        assert!(entities.tags.contains(&"my-tag".to_string()));
        assert!(entities.tags.contains(&"another_tag".to_string()));
        assert!(entities.tags.contains(&"mixed-tag_here".to_string()));
    }

    #[test]
    fn test_results_sorted() {
        let content = "#zebra #apple #mango [[Zebra Link]] [[Apple Link]]";
        let entities = extract_entities(content, &PathBuf::from("note.md"), &[]);

        assert_eq!(entities.tags, vec!["apple", "mango", "zebra"]);
        assert_eq!(entities.links, vec!["apple link", "zebra link"]);
    }

    #[test]
    fn test_multiple_wikilinks_same_target() {
        let content = "See [[Note]] and also [[Note]] again";
        let entities = extract_entities(content, &PathBuf::from("note.md"), &[]);

        // Should be deduplicated
        assert_eq!(entities.links.len(), 1);
        assert_eq!(entities.links[0], "note");
    }

    #[test]
    fn test_tag_at_start_of_line() {
        let content = "#first-tag\n#second-tag";
        let entities = extract_entities(content, &PathBuf::from("note.md"), &[]);

        assert!(entities.tags.contains(&"first-tag".to_string()));
        assert!(entities.tags.contains(&"second-tag".to_string()));
    }

    #[test]
    fn test_date_links_filtered() {
        let content = "See [[2022-10-23]] and [[My Note]]";
        let entities = extract_entities(content, &PathBuf::from("note.md"), &[]);

        assert!(!entities.links.contains(&"2022-10-23".to_string()));
        assert!(entities.links.contains(&"my note".to_string()));
    }

    #[test]
    fn test_readwise_source_tags_filtered() {
        let frontmatter_tags = vec![
            "readwise/source/reader".to_string(),
            "pkm".to_string(),
            "readwise/source/kindle".to_string(),
        ];
        let entities = extract_entities("content", &PathBuf::from("note.md"), &frontmatter_tags);

        assert!(
            !entities
                .tags
                .contains(&"readwise/source/reader".to_string())
        );
        assert!(
            !entities
                .tags
                .contains(&"readwise/source/kindle".to_string())
        );
        assert!(entities.tags.contains(&"pkm".to_string()));
    }

    #[test]
    fn test_is_date_link() {
        assert!(is_date_link("2022-10-23"));
        assert!(is_date_link("2024-01-01"));
        assert!(!is_date_link("My Note"));
        assert!(!is_date_link("2022-10"));
        assert!(!is_date_link("2022-10-23 extra"));
        assert!(!is_date_link("not-a-date"));
    }

    #[test]
    fn test_numeric_only_tags_filtered() {
        let frontmatter_tags = vec!["005".to_string(), "15".to_string(), "pkm".to_string()];
        let entities = extract_entities("content", &PathBuf::from("note.md"), &frontmatter_tags);

        assert!(!entities.tags.contains(&"005".to_string()));
        assert!(!entities.tags.contains(&"15".to_string()));
        assert!(entities.tags.contains(&"pkm".to_string()));
    }

    #[test]
    fn test_hex_color_tags_filtered() {
        let content = "color: #ff0000 and #333 and #1a2b3c but not #rust or #ff0000ff";
        let entities = extract_entities(content, &PathBuf::from("note.md"), &[]);

        assert!(!entities.tags.contains(&"ff0000".to_string()));
        assert!(!entities.tags.contains(&"333".to_string()));
        assert!(!entities.tags.contains(&"1a2b3c".to_string()));
        assert!(!entities.tags.contains(&"ff0000ff".to_string()));
        assert!(entities.tags.contains(&"rust".to_string()));
    }

    #[test]
    fn test_hex_color_not_overly_aggressive() {
        // Tags that happen to be short but contain non-hex chars should survive
        let content = "#git #npm #api";
        let entities = extract_entities(content, &PathBuf::from("note.md"), &[]);

        assert!(entities.tags.contains(&"git".to_string()));
        assert!(entities.tags.contains(&"npm".to_string()));
        assert!(entities.tags.contains(&"api".to_string()));
    }

    #[test]
    fn test_long_links_filtered() {
        let long_link = "a".repeat(201);
        let content = format!("[[{}]] [[Short Note]]", long_link);
        let entities = extract_entities(&content, &PathBuf::from("note.md"), &[]);

        assert_eq!(entities.links.len(), 1);
        assert!(entities.links.contains(&"short note".to_string()));
    }

    #[test]
    fn test_link_case_normalized() {
        let content = "[[My Note]] and [[MY NOTE]] and [[my note]]";
        let entities = extract_entities(content, &PathBuf::from("note.md"), &[]);

        // All three should normalize to one entry
        assert_eq!(entities.links.len(), 1);
        assert_eq!(entities.links[0], "my note");
    }
}
