//! Append-only log detection and chronological chunking.
//!
//! Two vault topologies look like "logs" but need opposite handling:
//!
//! - **Many log files** (`daily/2026-07-28.md`, `…-27.md`, …): each file is a
//!   short, coherent, dated unit. The existing occurrence-date + era + folder
//!   machinery already handles these; nothing here applies.
//! - **A single long append-only file** (`journal.md`): one file carrying an
//!   internal timeline of many dated entries under one document date. The era
//!   machinery can't see inside it, and its length pushes it onto the unbounded
//!   context paths. *This* is what the detector targets.
//!
//! Detection is deliberately structural, not semantic: "a single body with N+
//! monotonically-ordered internal date headers." A coherent essay — even a long
//! one that cites dates — lacks that signature, so this boundary is reliable in
//! a way that "essay vs log by vibes" is not.

use chrono::NaiveDate;

use super::chunking::chunk_document;

/// Minimum number of internal date headers before a body is considered a log.
const MIN_LOG_ENTRIES: usize = 3;

/// Fraction of adjacent entry-pairs that must agree on a direction for the body
/// to count as append-only (rather than scattered incidental dates).
const MONOTONIC_THRESHOLD: f64 = 0.8;

/// Whether the newest entry sits at the top or the bottom of the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogDirection {
    /// Entries appended at the bottom; newest is last (the common case).
    OldestFirst,
    /// Entries prepended at the top; newest is first (reverse-chronological).
    NewestFirst,
}

/// One dated entry within an append-only log body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub date: NaiveDate,
    /// Byte offset of the entry's header line within the body.
    pub start: usize,
}

/// A body recognised as an append-only log.
#[derive(Debug, Clone)]
pub struct AppendLog {
    pub direction: LogDirection,
    /// Entries in file order (not necessarily chronological — see `direction`).
    pub entries: Vec<LogEntry>,
}

/// Parse a leading `YYYY-MM-DD` / `YYYY/MM/DD` date from a header-ish line,
/// after stripping Markdown heading/list/quote/wikilink markers. High precision:
/// only fires when the date is the line's leading token.
fn parse_header_date(line: &str) -> Option<NaiveDate> {
    let s = line
        .trim_start()
        .trim_start_matches(|c| matches!(c, '#' | '-' | '*' | '>' | '[' | '_' | ' ' | '\t'));
    let b = s.as_bytes();
    if b.len() < 10 {
        return None;
    }
    let sep = b[4] as char;
    if (sep != '-' && sep != '/') || b[7] as char != sep {
        return None;
    }
    // Reject a longer digit run (e.g. `2026-12-3456`), which is not a date.
    if s[10..].starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let year: i32 = s.get(0..4)?.parse().ok()?;
    let month: u32 = s.get(5..7)?.parse().ok()?;
    let day: u32 = s.get(8..10)?.parse().ok()?;
    NaiveDate::from_ymd_opt(year, month, day)
}

/// Detect whether `body` (frontmatter already stripped) is an append-only log.
///
/// Returns `None` unless the body carries at least [`MIN_LOG_ENTRIES`] internal
/// date headers whose sequence is dominantly monotonic in one direction.
pub fn detect_append_log(body: &str) -> Option<AppendLog> {
    let mut entries: Vec<LogEntry> = Vec::new();
    let mut offset = 0usize;
    for line in body.split_inclusive('\n') {
        if let Some(date) = parse_header_date(line) {
            entries.push(LogEntry { date, start: offset });
        }
        offset += line.len();
    }
    if entries.len() < MIN_LOG_ENTRIES {
        return None;
    }

    let (mut inc, mut dec) = (0usize, 0usize);
    for w in entries.windows(2) {
        if w[1].date > w[0].date {
            inc += 1;
        } else if w[1].date < w[0].date {
            dec += 1;
        }
    }
    let total = inc + dec;
    if total == 0 {
        // All entries share one date — a template or a single day's notes, not
        // a timeline.
        return None;
    }
    let (direction, dominant) = if inc >= dec {
        (LogDirection::OldestFirst, inc)
    } else {
        (LogDirection::NewestFirst, dec)
    };
    if (dominant as f64) < MONOTONIC_THRESHOLD * total as f64 {
        return None;
    }
    Some(AppendLog { direction, entries })
}

/// Byte range `[start, end)` of the entry at file-order index `i`.
fn entry_span(body: &str, log: &AppendLog, i: usize) -> (usize, usize) {
    let start = log.entries[i].start;
    let end = log
        .entries
        .get(i + 1)
        .map(|e| e.start)
        .unwrap_or(body.len());
    (start, end)
}

/// Chunk an append-only log into chronological, entry-aligned chunks.
///
/// Each dated entry becomes a chunk (preserving file order in `chunk_index`, so
/// recency maps to chunk position via [`AppendLog::direction`]). Oversized
/// entries are split further with the generic chunker; the pre-header preamble
/// is dropped (typically a title). Falls back to the generic chunker if nothing
/// usable is produced.
pub fn chunk_append_log(body: &str, log: &AppendLog, chunk_size: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    for i in 0..log.entries.len() {
        let (start, end) = entry_span(body, log, i);
        let entry = body[start..end].trim();
        if entry.is_empty() {
            continue;
        }
        if entry.len() > chunk_size {
            chunks.extend(chunk_document(entry, chunk_size));
        } else {
            chunks.push(entry.to_string());
        }
    }
    if chunks.is_empty() {
        return chunk_document(body, chunk_size);
    }
    chunks
}

/// The most-recent entry's text, honouring append direction. Used as the bounded
/// representative context for a log's document-level occurrences (the highest-
/// value slice of a log, unlike an arbitrary head/tail excerpt). The caller is
/// responsible for any word-budget bounding.
pub fn most_recent_entry(body: &str, log: &AppendLog) -> String {
    let i = match log.direction {
        LogDirection::OldestFirst => log.entries.len() - 1,
        LogDirection::NewestFirst => 0,
    };
    let (start, end) = entry_span(body, log, i);
    body[start..end].trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log_body(dates: &[&str], filler_words: usize) -> String {
        let filler = "word ".repeat(filler_words);
        dates
            .iter()
            .map(|d| format!("## {d}\n{filler}\n"))
            .collect::<String>()
    }

    #[test]
    fn detects_bottom_append_log() {
        let body = log_body(&["2026-01-01", "2026-02-01", "2026-03-01", "2026-04-01"], 20);
        let log = detect_append_log(&body).expect("should detect a log");
        assert_eq!(log.direction, LogDirection::OldestFirst);
        assert_eq!(log.entries.len(), 4);
    }

    #[test]
    fn detects_top_append_reverse_chronological() {
        let body = log_body(&["2026-04-01", "2026-03-01", "2026-02-01", "2026-01-01"], 20);
        let log = detect_append_log(&body).expect("should detect a reverse log");
        assert_eq!(log.direction, LogDirection::NewestFirst);
    }

    #[test]
    fn rejects_coherent_note_with_scattered_dates() {
        // An essay citing several dates out of order is not a log.
        let body = "In 2019-05-01 we began. Earlier, 2011-01-02 mattered. \
                    Later still 2023-09-09 and back to 2005-03-03 for context. \
                    A long argument follows without any dated headers at all."
            .to_string();
        assert!(detect_append_log(&body).is_none());
    }

    #[test]
    fn rejects_too_few_entries() {
        let body = log_body(&["2026-01-01", "2026-02-01"], 20);
        assert!(detect_append_log(&body).is_none());
    }

    #[test]
    fn rejects_all_same_date() {
        let body = log_body(&["2026-01-01", "2026-01-01", "2026-01-01"], 20);
        assert!(detect_append_log(&body).is_none());
    }

    #[test]
    fn header_date_parses_wikilink_and_bullet_forms() {
        assert!(parse_header_date("## [[2026-07-28]] standup").is_some());
        assert!(parse_header_date("- 2026/07/28 note").is_some());
        assert!(parse_header_date("### 2026-07-28").is_some());
        assert!(parse_header_date("plain prose 2026-07-28 mid-line").is_none());
        assert!(parse_header_date("## 2026-13-40 bad date").is_none());
    }

    #[test]
    fn chunks_align_to_entries_in_order() {
        let body = log_body(&["2026-01-01", "2026-02-01", "2026-03-01"], 20);
        let log = detect_append_log(&body).unwrap();
        let chunks = chunk_append_log(&body, &log, 2048);
        assert_eq!(chunks.len(), 3);
        assert!(chunks[0].contains("2026-01-01"));
        assert!(chunks[2].contains("2026-03-01"));
    }

    #[test]
    fn most_recent_entry_follows_direction() {
        let bottom = log_body(&["2026-01-01", "2026-02-01", "2026-03-01"], 5);
        let log = detect_append_log(&bottom).unwrap();
        assert!(most_recent_entry(&bottom, &log).contains("2026-03-01"));

        let top = log_body(&["2026-03-01", "2026-02-01", "2026-01-01"], 5);
        let log = detect_append_log(&top).unwrap();
        assert!(most_recent_entry(&top, &log).contains("2026-03-01"));
    }
}
