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
use super::datepath::{month_from_token, parse_day_comma_year, parse_leading_year};

/// Minimum number of internal date headers before a body is considered a log.
/// Also the auto-detect bar for entries with real bodies (see
/// [`entries_with_body`] and the processor's auto-flag gate).
pub const MIN_LOG_ENTRIES: usize = 3;

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

/// Parse a leading date from a header-ish line, after stripping Markdown
/// heading/list/quote/wikilink markers. High precision: only fires when the
/// date is the line's leading token — dates cited mid-sentence never match.
/// Recognised forms, tried in order:
///
/// - `YYYY-MM-DD` / `YYYY/MM/DD` (ISO-ish)
/// - `MM/DD/YYYY` / `MM-DD-YYYY` (US ordering assumed; a first component > 12
///   cannot be a month and flips the reading to day-first, e.g. `28/07/2026`)
/// - `July 28, 2026` / `Jul 28 2026` (month name, full or abbreviated, comma
///   optional)
/// - `28 July 2026` (day-first with month name)
///
/// Month vocabulary and day/year parsing are shared with [`super::datepath`]
/// so header dates and file-path dates agree on what counts as a date.
fn parse_header_date(line: &str) -> Option<NaiveDate> {
    let s = line
        .trim_start()
        .trim_start_matches(|c| matches!(c, '#' | '-' | '*' | '>' | '[' | '_' | ' ' | '\t'));
    let s = strip_weekday_prefix(s);
    parse_iso_header(s)
        .or_else(|| parse_numeric_mdy(s))
        .or_else(|| parse_month_name_header(s))
        .or_else(|| parse_day_month_year(s))
}

/// Skip a leading weekday token (`Monday, February 16, 2026`, `Tue 2026-02-17`)
/// so the standard export/transcript header style parses. Only a recognized
/// weekday name is skipped — arbitrary leading words still (correctly) block
/// a date from counting as the line's leading token.
fn strip_weekday_prefix(s: &str) -> &str {
    const WEEKDAYS: [&str; 14] = [
        "monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday", "mon",
        "tue", "wed", "thu", "fri", "sat", "sun",
    ];
    let token_end = s
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(s.len());
    if WEEKDAYS.contains(&s[..token_end].to_ascii_lowercase().as_str()) {
        s[token_end..].trim_start_matches([',', ' ', '\t'])
    } else {
        s
    }
}

/// `YYYY-MM-DD` / `YYYY/MM/DD` at the head of the (marker-stripped) line.
fn parse_iso_header(s: &str) -> Option<NaiveDate> {
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

/// Leading run of 1..=`max` ASCII digits → (value, rest after the digits).
fn take_digits(s: &str, max: usize) -> Option<(u32, &str)> {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if end == 0 || end > max {
        return None;
    }
    Some((s[..end].parse().ok()?, &s[end..]))
}

/// `MM/DD/YYYY` or `MM-DD-YYYY` (matching separators) at the head of the line.
/// US ordering is assumed; when the first component exceeds 12 — so it cannot
/// be a month — the pair is read day-first instead (`28/07/2026`).
fn parse_numeric_mdy(s: &str) -> Option<NaiveDate> {
    let (a, rest) = take_digits(s, 2)?;
    let sep = rest.chars().next()?;
    if sep != '/' && sep != '-' {
        return None;
    }
    let (b, rest) = take_digits(&rest[1..], 2)?;
    let rest = rest.strip_prefix(sep)?;
    let year = parse_leading_year(rest)?;
    let (month, day) = if a <= 12 { (a, b) } else { (b, a) };
    NaiveDate::from_ymd_opt(year, month, day)
}

/// `July 28, 2026` / `Jul 28 2026`: month token first, then day and year.
fn parse_month_name_header(s: &str) -> Option<NaiveDate> {
    let token_end = s.find(char::is_whitespace).unwrap_or(s.len());
    let month = month_from_token(&s[..token_end])?;
    parse_day_comma_year(&s[token_end..], month)
}

/// `28 July 2026`: day first, then month name, then year.
fn parse_day_month_year(s: &str) -> Option<NaiveDate> {
    let (day, rest) = take_digits(s, 2)?;
    if !(1..=31).contains(&day) || !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = rest.trim_start();
    let token_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let month = month_from_token(&rest[..token_end])?;
    let year = parse_leading_year(rest[token_end..].trim_start_matches([' ', '\t', ',']))?;
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
            entries.push(LogEntry {
                date,
                start: offset,
            });
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

/// Number of entries carrying a non-empty body: real content on the lines
/// below the dated header line.
///
/// This is the structural discriminator between a journal and a stub note
/// that merely *lists* dates — a page of `[[2026-07-27]]` backlinks or a bare
/// dated scaffold parses as N monotonic headers, but every "entry" is a single
/// line with nothing under it. A real append-only log writes prose beneath its
/// date headers. Callers (the processor's auto-flag gate) require
/// [`MIN_LOG_ENTRIES`] such entries before flagging without an explicit
/// `type:` declaration.
pub fn entries_with_body(body: &str, log: &AppendLog) -> usize {
    (0..log.entries.len())
        .filter(|&i| {
            let (start, end) = entry_span(body, log, i);
            body[start..end]
                .split_once('\n')
                .is_some_and(|(_, below)| !below.trim().is_empty())
        })
        .count()
}

/// A chunk carrying the date of the log entry it was cut from.
#[derive(Debug, Clone)]
pub struct DatedChunk {
    /// Entry date in milliseconds since epoch (midnight UTC), `None` when the
    /// chunk came from the generic fallback rather than a dated entry.
    pub entry_date: Option<i64>,
    pub content: String,
}

fn date_millis(date: NaiveDate) -> i64 {
    date.and_hms_opt(0, 0, 0)
        .expect("midnight is always valid")
        .and_utc()
        .timestamp_millis()
}

/// Chunk an append-only log into chronological, entry-aligned chunks.
///
/// Each dated entry becomes a chunk (preserving file order in `chunk_index`, so
/// recency maps to chunk position via [`AppendLog::direction`]) and carries its
/// entry's date. Oversized entries are split further with the generic chunker,
/// every piece keeping the entry's date; the pre-header preamble is dropped
/// (typically a title). Falls back to the generic chunker — undated chunks —
/// if nothing usable is produced.
pub fn chunk_append_log_dated(body: &str, log: &AppendLog, chunk_size: usize) -> Vec<DatedChunk> {
    let mut chunks = Vec::new();
    for i in 0..log.entries.len() {
        let (start, end) = entry_span(body, log, i);
        let entry = body[start..end].trim();
        if entry.is_empty() {
            continue;
        }
        let entry_date = Some(date_millis(log.entries[i].date));
        if entry.len() > chunk_size {
            chunks.extend(
                chunk_document(entry, chunk_size)
                    .into_iter()
                    .map(|content| DatedChunk {
                        entry_date,
                        content,
                    }),
            );
        } else {
            chunks.push(DatedChunk {
                entry_date,
                content: entry.to_string(),
            });
        }
    }
    if chunks.is_empty() {
        return chunk_document(body, chunk_size)
            .into_iter()
            .map(|content| DatedChunk {
                entry_date: None,
                content,
            })
            .collect();
    }
    chunks
}

/// [`chunk_append_log_dated`] without the dates, for callers that only need
/// the text.
pub fn chunk_append_log(body: &str, log: &AppendLog, chunk_size: usize) -> Vec<String> {
    chunk_append_log_dated(body, log, chunk_size)
        .into_iter()
        .map(|chunk| chunk.content)
        .collect()
}

/// Every dated entry's `(date_millis, text)` in file order. Empty entries
/// (a dated header with nothing under it) are skipped. This is the
/// per-entry view used to emit one entity occurrence per log entry.
pub fn log_entries(body: &str, log: &AppendLog) -> Vec<(i64, String)> {
    (0..log.entries.len())
        .filter_map(|i| {
            let (start, end) = entry_span(body, log, i);
            let text = body[start..end].trim();
            if text.is_empty() {
                return None;
            }
            Some((date_millis(log.entries[i].date), text.to_string()))
        })
        .collect()
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
    fn weekday_prefixed_headers_detect() {
        // The standard export/transcript style: `### Monday, February 16, 2026`.
        let filler = "word ".repeat(30);
        let body = [
            "Monday, February 16, 2026",
            "Tuesday, February 17, 2026",
            "Sat 2026-02-21",
        ]
        .iter()
        .map(|d| format!("### {d}\n{filler}\n"))
        .collect::<String>();
        let log = detect_append_log(&body).expect("weekday-prefixed log should detect");
        assert_eq!(log.entries.len(), 3);
        assert_eq!(
            log.entries[0].date,
            NaiveDate::from_ymd_opt(2026, 2, 16).unwrap()
        );
        assert_eq!(
            log.entries[2].date,
            NaiveDate::from_ymd_opt(2026, 2, 21).unwrap()
        );
    }

    #[test]
    fn non_weekday_leading_word_still_blocks() {
        // A date cited after an arbitrary word is not a header date.
        let filler = "word ".repeat(30);
        let body = ["Deadline 2026-02-16", "Deadline 2026-02-17", "Deadline 2026-02-21"]
            .iter()
            .map(|d| format!("### {d}\n{filler}\n"))
            .collect::<String>();
        assert!(detect_append_log(&body).is_none());
    }

    #[test]
    fn dated_chunks_carry_entry_dates() {
        let body = log_body(&["2026-01-01", "2026-02-01", "2026-03-01"], 20);
        let log = detect_append_log(&body).expect("should detect a log");
        let chunks = chunk_append_log_dated(&body, &log, 2048);
        assert_eq!(chunks.len(), 3);
        let expected: Vec<i64> = ["2026-01-01", "2026-02-01", "2026-03-01"]
            .iter()
            .map(|d| {
                NaiveDate::parse_from_str(d, "%Y-%m-%d")
                    .unwrap()
                    .and_hms_opt(0, 0, 0)
                    .unwrap()
                    .and_utc()
                    .timestamp_millis()
            })
            .collect();
        for (chunk, want) in chunks.iter().zip(&expected) {
            assert_eq!(chunk.entry_date, Some(*want));
        }
    }

    #[test]
    fn oversized_entry_subchunks_share_entry_date() {
        // One entry far above the chunk size splits into several chunks that
        // must all keep that entry's date.
        let body = log_body(&["2026-01-01", "2026-02-01", "2026-03-01"], 200);
        let log = detect_append_log(&body).expect("should detect a log");
        let chunks = chunk_append_log_dated(&body, &log, 256);
        assert!(chunks.len() > 3, "large entries should split");
        assert!(chunks.iter().all(|c| c.entry_date.is_some()));
        let distinct: std::collections::HashSet<i64> =
            chunks.iter().filter_map(|c| c.entry_date).collect();
        assert_eq!(distinct.len(), 3, "sub-chunks share their entry's date");
    }

    #[test]
    fn detects_bottom_append_log() {
        let body = log_body(
            &["2026-01-01", "2026-02-01", "2026-03-01", "2026-04-01"],
            20,
        );
        let log = detect_append_log(&body).expect("should detect a log");
        assert_eq!(log.direction, LogDirection::OldestFirst);
        assert_eq!(log.entries.len(), 4);
    }

    #[test]
    fn detects_top_append_reverse_chronological() {
        let body = log_body(
            &["2026-04-01", "2026-03-01", "2026-02-01", "2026-01-01"],
            20,
        );
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

    // --- Multi-format header dates ---

    #[test]
    fn header_date_parses_full_month_name_with_comma() {
        let expect = NaiveDate::from_ymd_opt(2026, 7, 28).unwrap();
        assert_eq!(parse_header_date("## July 28, 2026"), Some(expect));
        assert_eq!(
            parse_header_date("## [[July 28, 2026]] standup"),
            Some(expect)
        );
    }

    #[test]
    fn header_date_parses_abbreviated_month_no_comma() {
        let expect = NaiveDate::from_ymd_opt(2026, 7, 28).unwrap();
        assert_eq!(parse_header_date("### Jul 28 2026"), Some(expect));
        assert_eq!(parse_header_date("- Jul. 28 2026 notes"), Some(expect));
        assert_eq!(
            parse_header_date("## Sept 3 2026"),
            NaiveDate::from_ymd_opt(2026, 9, 3)
        );
    }

    #[test]
    fn header_date_parses_numeric_us_order() {
        let expect = NaiveDate::from_ymd_opt(2026, 7, 28).unwrap();
        assert_eq!(parse_header_date("## 07/28/2026"), Some(expect));
        assert_eq!(parse_header_date("## 7/28/2026"), Some(expect));
        assert_eq!(parse_header_date("## 07-28-2026"), Some(expect));
    }

    #[test]
    fn header_date_numeric_day_first_when_month_impossible() {
        // First component > 12 cannot be a month → read day-first.
        let expect = NaiveDate::from_ymd_opt(2026, 7, 28).unwrap();
        assert_eq!(parse_header_date("## 28/07/2026"), Some(expect));
        assert_eq!(parse_header_date("## 28-07-2026"), Some(expect));
        // Both components > 12 is not a date at all.
        assert!(parse_header_date("## 28/28/2026").is_none());
    }

    #[test]
    fn header_date_parses_day_month_year() {
        let expect = NaiveDate::from_ymd_opt(2026, 7, 28).unwrap();
        assert_eq!(parse_header_date("## 28 July 2026"), Some(expect));
        assert_eq!(parse_header_date("## 28 Jul 2026 retro"), Some(expect));
    }

    #[test]
    fn header_date_rejects_near_misses() {
        // A month-ish word that is not a month.
        assert!(parse_header_date("## Julia 28, 2026").is_none());
        // Month name without a day+year is a heading, not a dated entry.
        assert!(parse_header_date("## March meeting notes").is_none());
        assert!(parse_header_date("## July 2026 planning").is_none());
        // Mid-line dates never fire.
        assert!(parse_header_date("we shipped on July 28, 2026").is_none());
        assert!(parse_header_date("ratio was 7/28/2026 units").is_none());
        // Numeric forms need a real 4-digit year.
        assert!(parse_header_date("## 7/28/26").is_none());
        assert!(parse_header_date("## 12-3456").is_none());
        // Mixed separators are not a date.
        assert!(parse_header_date("## 07/28-2026").is_none());
    }

    #[test]
    fn detects_log_with_month_name_headers() {
        let filler = "word ".repeat(20);
        let body = ["July 26, 2026", "July 27, 2026", "July 28, 2026"]
            .iter()
            .map(|d| format!("## {d}\n{filler}\n"))
            .collect::<String>();
        let log = detect_append_log(&body).expect("month-name log should detect");
        assert_eq!(log.direction, LogDirection::OldestFirst);
        assert_eq!(log.entries.len(), 3);
    }

    #[test]
    fn detects_log_with_mixed_date_formats() {
        // A journal whose author drifted between conventions still reads as
        // one monotonic timeline.
        let filler = "word ".repeat(20);
        let body = ["2026-07-25", "July 26, 2026", "07/27/2026", "28 Jul 2026"]
            .iter()
            .map(|d| format!("## {d}\n{filler}\n"))
            .collect::<String>();
        let log = detect_append_log(&body).expect("mixed-format log should detect");
        assert_eq!(log.direction, LogDirection::OldestFirst);
        assert_eq!(log.entries.len(), 4);
    }

    #[test]
    fn mixed_formats_still_reject_non_monotonic() {
        // New formats widen recall; the monotonic gate still holds precision.
        let filler = "word ".repeat(20);
        let body = ["July 28, 2026", "2026-01-05", "07/04/2026", "2 Feb 2026"]
            .iter()
            .map(|d| format!("## {d}\n{filler}\n"))
            .collect::<String>();
        assert!(detect_append_log(&body).is_none());
    }

    // --- Non-empty-body gate ---

    #[test]
    fn entries_with_body_counts_real_entries() {
        let body = log_body(&["2026-01-01", "2026-02-01", "2026-03-01"], 10);
        let log = detect_append_log(&body).unwrap();
        assert_eq!(entries_with_body(&body, &log), 3);
    }

    #[test]
    fn entries_with_body_is_zero_for_dated_line_list() {
        // A stub note listing dated backlinks parses as headers, but every
        // "entry" is a single line with nothing under it.
        let body = "# Days\n\n- [[2026-07-25]]\n- [[2026-07-26]]\n- [[2026-07-27]]\n";
        let log = detect_append_log(body).expect("structure alone still parses");
        assert_eq!(entries_with_body(body, &log), 0);
    }

    #[test]
    fn entries_with_body_ignores_blank_only_entries() {
        let body = "## 2026-01-01\n\n## 2026-02-01\nreal prose here\n## 2026-03-01\n\n\n";
        let log = detect_append_log(body).unwrap();
        assert_eq!(entries_with_body(body, &log), 1);
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
