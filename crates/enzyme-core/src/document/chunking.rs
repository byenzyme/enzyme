/// Default chunk size in approximate characters (~512 tokens at ~4 chars/token).
pub const DEFAULT_CHUNK_SIZE_CHARS: usize = 2048;

/// Minimum chunk size in characters. Chunks smaller than this are merged into
/// the next chunk to avoid near-empty embeddings (e.g. lone headings).
const MIN_CHUNK_SIZE_CHARS: usize = 100;

/// Split a document into chunks suitable for embedding.
///
/// Splitting strategy:
/// 1. Split on paragraph boundaries (double newlines) when possible.
/// 2. If a paragraph exceeds `chunk_size`, split on sentence boundaries.
/// 3. If a sentence still exceeds `chunk_size`, split at `chunk_size` on a word boundary.
/// 4. Merge any trailing chunk smaller than `MIN_CHUNK_SIZE_CHARS` into the previous chunk.
///
/// `chunk_size` is in characters (approximate ~4 chars per token, so 2048 chars ~ 512 tokens).
pub fn chunk_document(content: &str, chunk_size: usize) -> Vec<String> {
    let chunk_size = if chunk_size == 0 {
        DEFAULT_CHUNK_SIZE_CHARS
    } else {
        chunk_size
    };

    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    // If content fits in one chunk, return it directly.
    if trimmed.len() <= chunk_size {
        return vec![trimmed.to_string()];
    }

    // Split into paragraphs on double newlines.
    let paragraphs: Vec<&str> = trimmed.split("\n\n").collect();

    let mut chunks: Vec<String> = Vec::new();
    let mut current_chunk = String::new();

    for paragraph in &paragraphs {
        let para = paragraph.trim();
        if para.is_empty() {
            continue;
        }

        // If adding this paragraph would exceed the limit, finalize the current chunk.
        let would_be_len = if current_chunk.is_empty() {
            para.len()
        } else {
            current_chunk.len() + 2 + para.len() // +2 for "\n\n" separator
        };

        if would_be_len <= chunk_size {
            if !current_chunk.is_empty() {
                current_chunk.push_str("\n\n");
            }
            current_chunk.push_str(para);
        } else {
            // Flush current chunk if non-empty.
            if !current_chunk.is_empty() {
                chunks.push(current_chunk.clone());
                current_chunk.clear();
            }

            // If the paragraph itself exceeds chunk_size, split it further.
            if para.len() > chunk_size {
                let sub_chunks = split_large_text(para, chunk_size);
                // Add all but the last sub-chunk as standalone chunks.
                for (i, sc) in sub_chunks.iter().enumerate() {
                    if i < sub_chunks.len() - 1 {
                        chunks.push(sc.clone());
                    } else {
                        current_chunk = sc.clone();
                    }
                }
            } else {
                current_chunk = para.to_string();
            }
        }
    }

    // Flush remaining content.
    if !current_chunk.is_empty() {
        chunks.push(current_chunk);
    }

    // Merge undersized chunks into their neighbor to avoid near-empty embeddings
    // (e.g. lone headings like "# James 3").
    merge_small_chunks(&mut chunks, MIN_CHUNK_SIZE_CHARS);

    chunks
}

/// Merge chunks smaller than `min_size` into the next chunk (or previous if last).
fn merge_small_chunks(chunks: &mut Vec<String>, min_size: usize) {
    if chunks.len() <= 1 {
        return;
    }
    let mut merged: Vec<String> = Vec::with_capacity(chunks.len());
    let mut carry = String::new();

    for chunk in chunks.drain(..) {
        if !carry.is_empty() {
            // Prepend the carried small chunk
            carry.push_str("\n\n");
            carry.push_str(&chunk);
            if carry.len() >= min_size {
                merged.push(carry);
                carry = String::new();
            }
            // else keep carrying (still too small after merge)
        } else if chunk.len() < min_size {
            carry = chunk;
        } else {
            merged.push(chunk);
        }
    }

    // If there's still a carried chunk, append it to the last merged chunk
    if !carry.is_empty() {
        if let Some(last) = merged.last_mut() {
            last.push_str("\n\n");
            last.push_str(&carry);
        } else {
            // All chunks were tiny — just keep them as one
            merged.push(carry);
        }
    }

    *chunks = merged;
}

/// Split a large block of text (that exceeds `chunk_size`) by sentence boundaries,
/// falling back to word boundaries if individual sentences are too long.
fn split_large_text(text: &str, chunk_size: usize) -> Vec<String> {
    let sentences = split_sentences(text);
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();

    for sentence in &sentences {
        let sentence = sentence.trim();
        if sentence.is_empty() {
            continue;
        }

        let would_be_len = if current.is_empty() {
            sentence.len()
        } else {
            current.len() + 1 + sentence.len() // +1 for space
        };

        if would_be_len <= chunk_size {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(sentence);
        } else {
            if !current.is_empty() {
                chunks.push(current.clone());
                current.clear();
            }

            // If a single sentence exceeds chunk_size, split on word boundaries.
            if sentence.len() > chunk_size {
                let word_chunks = split_on_words(sentence, chunk_size);
                for (i, wc) in word_chunks.iter().enumerate() {
                    if i < word_chunks.len() - 1 {
                        chunks.push(wc.clone());
                    } else {
                        current = wc.clone();
                    }
                }
            } else {
                current = sentence.to_string();
            }
        }
    }

    if !current.is_empty() {
        chunks.push(current);
    }

    chunks
}

/// Approximate sentence splitting. Splits on `. `, `! `, `? ` followed by
/// an uppercase letter or end of text. Falls back to newline boundaries.
fn split_sentences(text: &str) -> Vec<String> {
    let mut sentences = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        current.push(chars[i]);

        // Check for sentence-ending punctuation followed by whitespace.
        if (chars[i] == '.' || chars[i] == '!' || chars[i] == '?') && i + 1 < len {
            if chars[i + 1].is_whitespace() {
                // Check if next non-whitespace char is uppercase or we're at the end.
                let next_nonws = chars[i + 1..].iter().find(|c| !c.is_whitespace());
                if next_nonws.is_none() || next_nonws.is_some_and(|c| c.is_uppercase()) {
                    sentences.push(current.trim().to_string());
                    current.clear();
                    i += 1;
                    continue;
                }
            }
        }

        // Also split on newlines (paragraph-level fallback)
        if chars[i] == '\n' {
            if !current.trim().is_empty() {
                sentences.push(current.trim().to_string());
            }
            current.clear();
        }

        i += 1;
    }

    if !current.trim().is_empty() {
        sentences.push(current.trim().to_string());
    }

    sentences
}

/// Split text on word boundaries to fit within `chunk_size` characters.
fn split_on_words(text: &str, chunk_size: usize) -> Vec<String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut chunks = Vec::new();
    let mut current = String::new();

    for word in &words {
        let would_be_len = if current.is_empty() {
            word.len()
        } else {
            current.len() + 1 + word.len()
        };

        if would_be_len <= chunk_size {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        } else {
            if !current.is_empty() {
                chunks.push(current.clone());
                current.clear();
            }
            // If a single word exceeds chunk_size, add it as its own chunk
            current = word.to_string();
        }
    }

    if !current.is_empty() {
        chunks.push(current);
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_short_document_single_chunk() {
        let content = "This is a short document.";
        let chunks = chunk_document(content, DEFAULT_CHUNK_SIZE_CHARS);

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], content);
    }

    #[test]
    fn test_empty_document() {
        let chunks = chunk_document("", DEFAULT_CHUNK_SIZE_CHARS);
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_whitespace_only_document() {
        let chunks = chunk_document("   \n\n  \n  ", DEFAULT_CHUNK_SIZE_CHARS);
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_respects_paragraph_boundaries() {
        // Create two paragraphs that each fit in a chunk but together don't.
        let para1 = "A".repeat(100);
        let para2 = "B".repeat(100);
        let content = format!("{}\n\n{}", para1, para2);

        // With chunk_size=150, each paragraph fits individually but not together (200+2 > 150).
        let chunks = chunk_document(&content, 150);

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], para1);
        assert_eq!(chunks[1], para2);
    }

    #[test]
    fn test_merges_small_paragraphs() {
        let content = "Paragraph 1.\n\nParagraph 2.\n\nParagraph 3.";
        // With a large chunk size, all paragraphs should be in one chunk.
        let chunks = chunk_document(content, 10000);

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], content);
    }

    #[test]
    fn test_long_paragraph_split_on_sentences() {
        // Create a paragraph with multiple sentences that exceeds chunk_size.
        let sentence = "This is a test sentence. ";
        let paragraph = sentence.repeat(50); // ~1250 chars
        let content = paragraph.trim().to_string();

        let chunks = chunk_document(&content, 200);

        assert!(chunks.len() > 1, "should split into multiple chunks");
        for chunk in &chunks {
            // Each chunk should be at most chunk_size (with some tolerance for word boundaries)
            assert!(
                chunk.len() <= 250,
                "chunk too long ({} chars): {:?}",
                chunk.len(),
                &chunk[..50.min(chunk.len())]
            );
        }
    }

    #[test]
    fn test_all_content_preserved() {
        let content = "First paragraph with some content.\n\nSecond paragraph here.\n\nThird one.";
        let chunks = chunk_document(content, 40);

        // Verify all content appears somewhere in the chunks
        let joined = chunks.join(" ");
        assert!(joined.contains("First paragraph"));
        assert!(joined.contains("Second paragraph"));
        assert!(joined.contains("Third one"));
    }

    #[test]
    fn test_default_chunk_size() {
        let content = "Short content.";
        let chunks = chunk_document(content, 0); // 0 should use default
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn test_chunks_are_trimmed() {
        let content = "  \n\nSome content.  \n\n  More content.  \n\n";
        let chunks = chunk_document(content, 10000);

        for chunk in &chunks {
            assert_eq!(chunk, chunk.trim(), "chunks should be trimmed");
        }
    }

    #[test]
    fn test_multiple_paragraphs_boundary() {
        // Use paragraphs large enough to exceed MIN_CHUNK_SIZE_CHARS individually.
        let p1 = format!(
            "Alpha paragraph with enough content to stand alone. {}",
            "x".repeat(80)
        );
        let p2 = format!(
            "Beta paragraph with enough content to stand alone. {}",
            "y".repeat(80)
        );
        let p3 = format!(
            "Gamma paragraph with enough content to stand alone. {}",
            "z".repeat(80)
        );
        let content = format!("{}\n\n{}\n\n{}", p1, p2, p3);

        // chunk_size fits p1+p2 but not p1+p2+p3
        let chunks = chunk_document(&content, 300);

        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].contains("Alpha"));
        assert!(chunks[0].contains("Beta"));
        assert!(chunks[1].contains("Gamma"));
    }

    #[test]
    fn test_small_heading_merged_into_next_chunk() {
        // A heading alone should be merged into the following paragraph.
        let content = format!(
            "# Chapter Title\n\n{}",
            "Some real content here. ".repeat(10)
        );
        let chunks = chunk_document(&content, 2048);

        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].contains("# Chapter Title"));
        assert!(chunks[0].contains("Some real content"));
    }

    #[test]
    fn test_very_long_word() {
        // A word longer than chunk_size should still produce a chunk.
        let long_word = "a".repeat(300);
        let content = format!("Hello {} world", long_word);

        let chunks = chunk_document(&content, 100);
        assert!(!chunks.is_empty());

        // The long word should appear in some chunk
        let joined = chunks.join("");
        assert!(joined.contains(&long_word));
    }
}
