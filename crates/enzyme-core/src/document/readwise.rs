/// Readwise-aware chunking for highlight-based content.
///
/// Readwise exports (podcasts, articles, books) contain `## Highlights` sections
/// with `^rw`-prefixed reference IDs and optional `**Note**:` annotations.
/// This module keeps highlight+note pairs as atomic units during chunking.

/// A single Readwise highlight with optional user annotation.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadwiseHighlight {
    pub text: String,
    pub note: Option<String>,
}

/// Check if content is Readwise-formatted (requires 2+ `^rw` markers).
pub fn is_readwise_content(content: &str) -> bool {
    let count = content.match_indices("^rw").count();
    count >= 2
}

/// Extract highlights from Readwise content.
///
/// Splits content after `## Highlights` by `^rw` markers. Each block is parsed
/// into highlight text + optional `**Note**:` annotation.
pub fn extract_highlights(content: &str) -> Vec<ReadwiseHighlight> {
    // Find the "## Highlights" section
    let (highlights_section, has_header) = if let Some(idx) = content.find("## Highlights") {
        (&content[idx..], true)
    } else {
        // If no ## Highlights header, process the whole content
        (content, false)
    };

    // Find all ^rw marker positions
    let rw_positions: Vec<usize> = highlights_section
        .match_indices("^rw")
        .map(|(pos, _)| pos)
        .collect();

    if rw_positions.is_empty() {
        return Vec::new();
    }

    let mut highlights = Vec::new();

    for (i, &pos) in rw_positions.iter().enumerate() {
        // The highlight block is the text BEFORE this ^rw marker,
        // starting from the previous ^rw marker (or from the start of the section).
        let block_start = if i == 0 {
            if has_header {
                // Skip past "## Highlights\n" header line
                highlights_section.find('\n').map(|p| p + 1).unwrap_or(0)
            } else {
                0
            }
        } else {
            // Start after the previous ^rw marker line
            let prev_pos = rw_positions[i - 1];
            // Find end of the ^rw marker line
            highlights_section[prev_pos..]
                .find('\n')
                .map(|p| prev_pos + p + 1)
                .unwrap_or(prev_pos + 3)
        };

        // block_start can exceed pos when ^rw markers appear inside code
        // blocks or on the same line — skip these malformed ranges.
        if block_start > pos {
            continue;
        }

        let block = highlights_section[block_start..pos].trim();

        if block.is_empty() {
            continue;
        }

        let highlight = parse_highlight_block(block);
        if !highlight.text.is_empty() {
            highlights.push(highlight);
        }
    }

    // Handle the last block (after the last ^rw marker)
    if let Some(&last_pos) = rw_positions.last() {
        let after_marker = highlights_section[last_pos..]
            .find('\n')
            .map(|p| last_pos + p + 1)
            .unwrap_or(highlights_section.len());

        if after_marker < highlights_section.len() {
            let remaining = highlights_section[after_marker..].trim();
            if !remaining.is_empty() && !remaining.starts_with("^rw") {
                let highlight = parse_highlight_block(remaining);
                if !highlight.text.is_empty() {
                    highlights.push(highlight);
                }
            }
        }
    }

    highlights
}

/// Parse a highlight block into text and optional note.
fn parse_highlight_block(block: &str) -> ReadwiseHighlight {
    // Look for **Note**: or **Note:** pattern
    if let Some(note_idx) = find_note_marker(block) {
        let text = block[..note_idx].trim().to_string();
        let note_text = block[note_idx..].trim().to_string();
        // Strip the **Note**: prefix
        let note_content = note_text
            .trim_start_matches("**Note**:")
            .trim_start_matches("**Note:**")
            .trim()
            .to_string();

        ReadwiseHighlight {
            text,
            note: if note_content.is_empty() {
                None
            } else {
                Some(note_content)
            },
        }
    } else {
        ReadwiseHighlight {
            text: block.to_string(),
            note: None,
        }
    }
}

/// Find the position of a `**Note**:` marker in the block.
fn find_note_marker(block: &str) -> Option<usize> {
    block
        .find("**Note**:")
        .or_else(|| block.find("**Note:**"))
}

/// Chunk Readwise highlights into groups, keeping highlight+note as atomic units.
///
/// Groups `per_chunk` highlights together. Adds context padding from adjacent
/// highlights (first/last `context_words` words).
pub fn chunk_readwise_highlights(
    highlights: &[ReadwiseHighlight],
    per_chunk: usize,
    context_words: usize,
) -> Vec<String> {
    if highlights.is_empty() {
        return Vec::new();
    }

    let per_chunk = if per_chunk == 0 { 5 } else { per_chunk };
    let mut chunks = Vec::new();

    for group_start in (0..highlights.len()).step_by(per_chunk) {
        let group_end = (group_start + per_chunk).min(highlights.len());
        let group = &highlights[group_start..group_end];

        let mut chunk_parts: Vec<String> = Vec::new();

        // Leading context from the highlight before this group
        if group_start > 0 {
            let prev = &highlights[group_start - 1];
            let ctx = last_n_words(&prev.text, context_words);
            if !ctx.is_empty() {
                chunk_parts.push(format!("[...] {}", ctx));
            }
        }

        // Main highlights in this group
        for h in group {
            let mut entry = h.text.clone();
            if let Some(ref note) = h.note {
                entry.push_str(&format!("\nNote: {}", note));
            }
            chunk_parts.push(entry);
        }

        // Trailing context from the highlight after this group
        if group_end < highlights.len() {
            let next = &highlights[group_end];
            let ctx = first_n_words(&next.text, context_words);
            if !ctx.is_empty() {
                chunk_parts.push(format!("{} [...]", ctx));
            }
        }

        chunks.push(chunk_parts.join("\n\n"));
    }

    chunks
}

/// Get the first N words of a string.
fn first_n_words(s: &str, n: usize) -> String {
    s.split_whitespace()
        .take(n)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Get the last N words of a string.
fn last_n_words(s: &str, n: usize) -> String {
    let words: Vec<&str> = s.split_whitespace().collect();
    let start = words.len().saturating_sub(n);
    words[start..].join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_readwise_content_true() {
        let content = "## Highlights\nSome highlight\n^rw123\nAnother highlight\n^rw456\n";
        assert!(is_readwise_content(content));
    }

    #[test]
    fn test_is_readwise_content_false_one_marker() {
        let content = "Some text with one ^rw123 marker\n";
        assert!(!is_readwise_content(content));
    }

    #[test]
    fn test_is_readwise_content_false_none() {
        let content = "Regular markdown content\n\nNo Readwise markers here.\n";
        assert!(!is_readwise_content(content));
    }

    #[test]
    fn test_extract_highlights_basic() {
        let content = "\
## Highlights
First highlight text here
^rw100
Second highlight text here
^rw200
";
        let highlights = extract_highlights(content);
        assert_eq!(highlights.len(), 2);
        assert_eq!(highlights[0].text, "First highlight text here");
        assert_eq!(highlights[1].text, "Second highlight text here");
        assert!(highlights[0].note.is_none());
    }

    #[test]
    fn test_extract_highlights_with_notes() {
        let content = "\
## Highlights
Some highlight text
**Note**: My personal annotation
^rw100
Another highlight
^rw200
";
        let highlights = extract_highlights(content);
        assert_eq!(highlights.len(), 2);
        assert_eq!(highlights[0].text, "Some highlight text");
        assert_eq!(
            highlights[0].note,
            Some("My personal annotation".to_string())
        );
        assert!(highlights[1].note.is_none());
    }

    #[test]
    fn test_extract_highlights_no_header() {
        let content = "\
First highlight
^rw100
Second highlight
^rw200
";
        let highlights = extract_highlights(content);
        assert_eq!(highlights.len(), 2);
    }

    #[test]
    fn test_chunk_readwise_highlights_basic() {
        let highlights = vec![
            ReadwiseHighlight {
                text: "Highlight one".to_string(),
                note: None,
            },
            ReadwiseHighlight {
                text: "Highlight two".to_string(),
                note: Some("Note for two".to_string()),
            },
            ReadwiseHighlight {
                text: "Highlight three".to_string(),
                note: None,
            },
        ];

        let chunks = chunk_readwise_highlights(&highlights, 2, 5);
        assert_eq!(chunks.len(), 2);

        // First chunk: highlights 0,1 + trailing context from highlight 2
        assert!(chunks[0].contains("Highlight one"));
        assert!(chunks[0].contains("Note: Note for two"));
        assert!(chunks[0].contains("Highlight three [...]"));

        // Second chunk: leading context from highlight 1 + highlight 2
        assert!(chunks[1].contains("[...] Highlight two"));
        assert!(chunks[1].contains("Highlight three"));
    }

    #[test]
    fn test_chunk_readwise_highlights_single_group() {
        let highlights = vec![
            ReadwiseHighlight {
                text: "Only highlight".to_string(),
                note: None,
            },
        ];

        let chunks = chunk_readwise_highlights(&highlights, 5, 10);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], "Only highlight");
    }

    #[test]
    fn test_chunk_readwise_highlights_empty() {
        let chunks = chunk_readwise_highlights(&[], 5, 10);
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_extract_highlights_with_embedded_code_blocks() {
        // Reproduces panic when ^rw markers appear inside enzyme/reason code blocks,
        // causing block_start > pos on the slice at line 65.
        let content = "\
## Highlights
Some highlight text
^rw100
```enzyme
Readwise/Books/<10 Readwise/Articles/<10
```

The notes highlight a clear tension ^rw200
Another normal highlight
^rw300
";
        // Should not panic; gracefully skips malformed ranges
        let highlights = extract_highlights(content);
        assert!(!highlights.is_empty());
    }

    #[test]
    fn test_extract_highlights_markers_on_same_line() {
        // ^rw markers back to back on same line — block_start will exceed pos
        let content = "## Highlights\nText before\n^rw100 ^rw200\nMore text\n^rw300\n";
        let highlights = extract_highlights(content);
        assert!(!highlights.is_empty());
    }

    #[test]
    fn test_first_n_words() {
        assert_eq!(first_n_words("one two three four five", 3), "one two three");
        assert_eq!(first_n_words("short", 10), "short");
        assert_eq!(first_n_words("", 5), "");
    }

    #[test]
    fn test_last_n_words() {
        assert_eq!(
            last_n_words("one two three four five", 3),
            "three four five"
        );
        assert_eq!(last_n_words("short", 10), "short");
        assert_eq!(last_n_words("", 5), "");
    }
}
