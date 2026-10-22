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

/// Size a log chunk accumulates whole entries up to before it is cut (~250
/// tokens). See [`chunk_append_log_dated`].
pub const MIN_LOG_CHUNK_CHARS: usize = 1000;

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

/// One entry within an append-only log body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    /// Entry date. Always `Some` for auto-detected logs. In a declared log
    /// (see [`segment_declared_log`]) it is borrowed from the nearest dated
    /// neighbour when the entry has none of its own, and `None` only when no
    /// entry in the file carried a parseable date.
    pub date: Option<NaiveDate>,
    /// True when `date` was borrowed from a neighbour rather than parsed from
    /// this entry.
    pub date_inferred: bool,
    /// Byte offset of the entry's header line within the body. An entry runs
    /// to the next entry's start (or the end of the body).
    pub start: usize,
}

/// A body recognised as an append-only log.
#[derive(Debug, Clone)]
pub struct AppendLog {
    pub direction: LogDirection,
    /// Entries in file order (not necessarily chronological — see `direction`).
    pub entries: Vec<LogEntry>,
}

impl AppendLog {
    /// Entries whose date was parsed from the entry itself (not borrowed).
    pub fn dated_entries(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.date.is_some() && !e.date_inferred)
            .count()
    }
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
    parse_builtin_header_date(header_text(line))
}

/// A header-ish line with its Markdown markers and any leading weekday
/// stripped, so a date can be read as its leading token. Shared with
/// configured heading formats (`super::daterules`).
pub(crate) fn header_text(line: &str) -> &str {
    let s = line
        .trim_start()
        .trim_start_matches(|c| matches!(c, '#' | '-' | '*' | '>' | '[' | '_' | ' ' | '\t'));
    strip_weekday_prefix(s)
}

/// The built-in header forms, read from already-stripped header text.
pub(crate) fn parse_builtin_header_date(s: &str) -> Option<NaiveDate> {
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
        "monday",
        "tuesday",
        "wednesday",
        "thursday",
        "friday",
        "saturday",
        "sunday",
        "mon",
        "tue",
        "wed",
        "thu",
        "fri",
        "sat",
        "sun",
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
    // Use `get` so byte offset 10 is never sliced inside a multibyte scalar.
    if s.get(10..)
        .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
    {
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
    detect_append_log_with(body, parse_header_date)
}

/// [`detect_append_log`] with a caller-supplied header-date reader, used when
/// a source declares its own heading formats.
pub fn detect_append_log_with(
    body: &str,
    header_date: impl Fn(&str) -> Option<NaiveDate>,
) -> Option<AppendLog> {
    let mut entries: Vec<LogEntry> = Vec::new();
    let mut offset = 0usize;
    for line in body.split_inclusive('\n') {
        if let Some(date) = header_date(line) {
            entries.push(LogEntry {
                date: Some(date),
                date_inferred: false,
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

/// ATX heading level (1–6) of a line, or `None` when it is not a heading.
/// `#tag` lines are not headings: the hashes must be followed by whitespace or
/// end the line.
fn heading_level(line: &str) -> Option<usize> {
    let level = line.bytes().take_while(|&b| b == b'#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &line[level..];
    (rest.is_empty() || rest.starts_with([' ', '\t', '\n', '\r'])).then_some(level)
}

/// A top-level (unindented) Markdown list item: `- `, `* `, `+ `, `1. `, `1) `.
fn is_top_level_item(line: &str) -> bool {
    if line.starts_with("- ") || line.starts_with("* ") || line.starts_with("+ ") {
        return true;
    }
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && line[digits..].starts_with(['.', ')']) && line[digits + 1..].starts_with(' ')
}

/// `YYYYMMDD` or `YYMMDD` (read year-first, two-digit years as 20YY) as an
/// exact leading digit run.
fn parse_compact_date(s: &str) -> Option<NaiveDate> {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (year, rest) = match end {
        8 => (s[..4].parse::<i32>().ok()?, &s[4..8]),
        6 => (2000 + s[..2].parse::<i32>().ok()?, &s[2..6]),
        _ => return None,
    };
    if !(1900..=2100).contains(&year) {
        return None;
    }
    NaiveDate::from_ymd_opt(year, rest[..2].parse().ok()?, rest[2..].parse().ok()?)
}

/// Date for a declared-log entry line. Dates in declared logs are
/// unpredictable, so this is looser than [`parse_header_date`]: the leading
/// token may also be compact (`261012`, `20261012`), and failing that the
/// first full date anywhere on the line counts (`… [[2023-02-21]]`). Compact
/// forms only count as the leading token, never mid-line, where six-digit
/// runs are usually IDs.
fn parse_declared_date(line: &str) -> Option<NaiveDate> {
    if let Some(date) = parse_header_date(line) {
        return Some(date);
    }
    let lead = line
        .trim_start()
        .trim_start_matches(|c| matches!(c, '#' | '-' | '*' | '+' | '>' | '[' | '_' | ' ' | '\t'));
    if let Some(date) = parse_compact_date(lead) {
        return Some(date);
    }
    let mut prev_alnum = false;
    for (i, c) in line.char_indices() {
        if c.is_alphanumeric() && !prev_alnum {
            let s = &line[i..];
            let date = parse_iso_header(s)
                .or_else(|| parse_numeric_mdy(s))
                .or_else(|| parse_month_name_header(s))
                .or_else(|| parse_day_month_year(s));
            if date.is_some() {
                return date;
            }
        }
        prev_alnum = c.is_alphanumeric();
    }
    None
}

/// One date component carried by a heading whose text is just a number or a
/// month name (`# 26`, `## 10`, `## October`, `### 12 rainy`).
fn heading_component(text: &str) -> Option<(u32, bool)> {
    let text = text.trim();
    let token_end = text
        .find(|c: char| !c.is_alphanumeric())
        .unwrap_or(text.len());
    let token = &text[..token_end];
    if let Some(month) = month_from_token(token) {
        return Some((month, true));
    }
    (1..=4)
        .contains(&token.len())
        .then(|| token.parse().ok())
        .flatten()
        .map(|n| (n, false))
}

/// Date spread over nested headings: year, month and day as the last three
/// headings on the path (`# 26` / `## 10` / `### 12`, `# 2026` / `## October`
/// / `### 12`).
fn compose_heading_date(path: &[&str]) -> Option<NaiveDate> {
    let [y, m, d] = path.get(path.len().checked_sub(3)?..)? else {
        return None;
    };
    let (year, year_is_name) = heading_component(y)?;
    let (month, _) = heading_component(m)?;
    let (day, day_is_name) = heading_component(d)?;
    if year_is_name || day_is_name {
        return None;
    }
    let year = match year {
        0..=99 => 2000 + year as i32,
        1900..=2100 => year as i32,
        _ => return None,
    };
    NaiveDate::from_ymd_opt(year, month, day)
}

/// Segment a log the user declared (config `log "name"`), where dates are
/// unpredictable and structure is only a heuristic.
///
/// Boundaries come from repetition, not dates. Candidates are each heading
/// level, top-level list items, and lines that lead with a date; each needs at
/// least [`MIN_LOG_ENTRIES`] entries. The candidate with the most dated
/// entries wins (ties: more entries, then headings before date lines before
/// list items). With no repeating structure, paragraphs are the entries.
/// Entries are dated where possible — a heading's own text or its nested
/// year/month/day path, otherwise the first date on the entry's first line —
/// and undated entries borrow the nearest dated neighbour's date. Direction
/// comes from the parsed dates, defaulting to newest-last when there are none.
///
/// There is no ordering threshold: the user's declaration, not structure, is
/// what makes this a log. Returns `None` only for an empty body.
pub fn segment_declared_log(body: &str) -> Option<AppendLog> {
    segment_declared_log_with(body, |_| None, true)
}

/// [`segment_declared_log`] for a source that declares heading formats
/// (`date entries … by headings as "…"`). `rule_date` reads a line's leading
/// date with the declared formats and is tried before the built-in readers;
/// `builtin` false (a rule marked `only`) turns the built-in readers off,
/// including nested year/month/day heading paths and compact leading dates.
/// Boundaries are chosen exactly as in [`segment_declared_log`].
pub fn segment_declared_log_with(
    body: &str,
    rule_date: impl Fn(&str) -> Option<NaiveDate>,
    builtin: bool,
) -> Option<AppendLog> {
    let declared_date = |text: &str| {
        rule_date(text).or_else(|| {
            if builtin {
                parse_declared_date(text)
            } else {
                None
            }
        })
    };
    struct Line<'a> {
        start: usize,
        text: &'a str,
        heading: Option<usize>,
        item: bool,
        blank: bool,
        indented: bool,
    }
    let mut lines = Vec::new();
    let mut offset = 0usize;
    let mut in_fence = false;
    for raw in body.split_inclusive('\n') {
        let text = raw.trim_end_matches(['\n', '\r']);
        let fence = text.trim_start().starts_with("```") || text.trim_start().starts_with("~~~");
        let structural = !in_fence && !fence;
        lines.push(Line {
            start: offset,
            text,
            heading: if structural {
                heading_level(text)
            } else {
                None
            },
            item: structural && is_top_level_item(text),
            blank: text.trim().is_empty(),
            indented: text.starts_with([' ', '\t']) || !structural,
        });
        if fence {
            in_fence = !in_fence;
        }
        offset += raw.len();
    }

    // Each heading's date: its own text, else its nested heading path.
    let mut path: Vec<(usize, &str)> = Vec::new();
    let heading_dates: Vec<Option<NaiveDate>> = lines
        .iter()
        .map(|line| {
            let level = line.heading?;
            path.retain(|&(l, _)| l < level);
            let text = line.text[level..].trim();
            path.push((level, text));
            declared_date(text).or_else(|| {
                if !builtin {
                    return None;
                }
                let texts: Vec<&str> = path.iter().map(|&(_, t)| t).collect();
                compose_heading_date(&texts)
            })
        })
        .collect();
    let entries_at =
        |starts: Vec<usize>, date: &dyn Fn(usize) -> Option<NaiveDate>| -> Vec<LogEntry> {
            starts
                .into_iter()
                .map(|i| LogEntry {
                    date: date(i),
                    date_inferred: false,
                    start: lines[i].start,
                })
                .collect()
        };
    let line_date = |i: usize| declared_date(lines[i].text);
    let starts = |keep: &dyn Fn(usize) -> bool| -> Vec<usize> {
        (0..lines.len()).filter(|&i| keep(i)).collect()
    };

    // (priority, entries) per candidate; lower priority wins ties.
    let mut candidates: Vec<(u8, Vec<LogEntry>)> = (1..=6)
        .map(|level| {
            let idx = starts(&|i| lines[i].heading == Some(level));
            (0, entries_at(idx, &|i| heading_dates[i]))
        })
        .collect();
    let date_led = starts(&|i| {
        let line = &lines[i];
        line.heading.is_none()
            && !line.indented
            && (rule_date(line.text).is_some()
                || builtin
                    && (parse_header_date(line.text).is_some()
                        || parse_compact_date(
                            line.text.trim_start_matches(['-', '*', '+', '>', ' ']),
                        )
                        .is_some()))
    });
    candidates.push((1, entries_at(date_led, &line_date)));
    candidates.push((2, entries_at(starts(&|i| lines[i].item), &line_date)));

    let best = candidates
        .into_iter()
        .filter(|(_, e)| e.len() >= MIN_LOG_ENTRIES)
        .min_by_key(|(priority, e)| {
            let dated = e.iter().filter(|x| x.date.is_some()).count();
            (
                std::cmp::Reverse(dated),
                std::cmp::Reverse(e.len()),
                *priority,
            )
        })
        .map(|(_, e)| e);
    let mut entries = best.unwrap_or_else(|| {
        let paragraphs = starts(&|i| !lines[i].blank && (i == 0 || lines[i - 1].blank));
        entries_at(paragraphs, &line_date)
    });
    if entries.is_empty() {
        return None;
    }

    let dated: Vec<(usize, NaiveDate)> = entries
        .iter()
        .enumerate()
        .filter_map(|(i, e)| e.date.map(|d| (i, d)))
        .collect();
    let (mut inc, mut dec) = (0usize, 0usize);
    for w in dated.windows(2) {
        if w[1].1 > w[0].1 {
            inc += 1;
        } else if w[1].1 < w[0].1 {
            dec += 1;
        }
    }
    let direction = if dec > inc {
        LogDirection::NewestFirst
    } else {
        LogDirection::OldestFirst
    };
    for (i, entry) in entries.iter_mut().enumerate() {
        if entry.date.is_some() {
            continue;
        }
        // Nearest dated neighbour by position; the earlier one on a tie.
        entry.date = dated
            .iter()
            .min_by_key(|&&(j, _)| (j.abs_diff(i), j > i))
            .map(|&(_, d)| d);
        entry.date_inferred = entry.date.is_some();
    }
    Some(AppendLog { direction, entries })
}

/// Byte range `[start, end)` of the entry at file-order index `i`.
fn entry_span(body: &str, log: &AppendLog, i: usize) -> (usize, usize) {
    let start = log.entries[i].start;
    let end = log.entries.get(i + 1).map_or(body.len(), |next| next.start);
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
#[derive(Debug, Clone, Default)]
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
/// Whole entries accumulate, in file order, until a chunk reaches
/// [`MIN_LOG_CHUNK_CHARS`], never exceeding `chunk_size`; an entry larger than
/// that is first split with the generic chunker and its pieces accumulate the
/// same way. Short
/// entries (a line a day, a single highlight) would otherwise make chunks too
/// small to carry meaning, and catalyst sampling takes a fixed number of
/// chunks, so tiny chunks mean less evidence per job. Each chunk carries the
/// newest date among its entries; a short tail merges into the previous chunk.
/// The pre-entry preamble (typically a title) is dropped. Falls back to the
/// generic chunker — undated chunks — if nothing usable is produced.
pub fn chunk_append_log_dated(body: &str, log: &AppendLog, chunk_size: usize) -> Vec<DatedChunk> {
    let min_chars = MIN_LOG_CHUNK_CHARS.min(chunk_size);
    let mut chunks: Vec<DatedChunk> = Vec::new();
    let mut buf = DatedChunk::default();
    let flush = |buf: &mut DatedChunk, chunks: &mut Vec<DatedChunk>| {
        if !buf.content.is_empty() {
            chunks.push(std::mem::take(buf));
        }
    };
    for (date, text) in log_entries(body, log) {
        let pieces = if text.len() > chunk_size {
            chunk_document(&text, chunk_size)
        } else {
            vec![text]
        };
        for piece in pieces {
            if !buf.content.is_empty() && buf.content.len() + 2 + piece.len() > chunk_size {
                flush(&mut buf, &mut chunks);
            }
            if !buf.content.is_empty() {
                buf.content.push_str("\n\n");
            }
            buf.content.push_str(&piece);
            buf.entry_date = buf.entry_date.max(date);
            if buf.content.len() >= min_chars {
                flush(&mut buf, &mut chunks);
            }
        }
    }
    if !buf.content.is_empty() {
        match chunks.last_mut() {
            Some(last) if last.content.len() + 2 + buf.content.len() <= chunk_size => {
                last.content.push_str("\n\n");
                last.content.push_str(&buf.content);
                last.entry_date = last.entry_date.max(buf.entry_date);
            }
            _ => chunks.push(buf),
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

/// Every entry's `(date_millis, text)` in file order. Empty entries (a header
/// with nothing under it) are skipped. The date is `None` only for a declared
/// log with no parseable dates anywhere. This is the per-entry view used to
/// emit one entity occurrence per log entry.
pub fn log_entries(body: &str, log: &AppendLog) -> Vec<(Option<i64>, String)> {
    (0..log.entries.len())
        .filter_map(|i| {
            let (start, end) = entry_span(body, log, i);
            let text = body[start..end].trim();
            if text.is_empty() {
                return None;
            }
            Some((log.entries[i].date.map(date_millis), text.to_string()))
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
        assert_eq!(log.entries[0].date, NaiveDate::from_ymd_opt(2026, 2, 16));
        assert_eq!(log.entries[2].date, NaiveDate::from_ymd_opt(2026, 2, 21));
    }

    #[test]
    fn non_weekday_leading_word_still_blocks() {
        // A date cited after an arbitrary word is not a header date.
        let filler = "word ".repeat(30);
        let body = [
            "Deadline 2026-02-16",
            "Deadline 2026-02-17",
            "Deadline 2026-02-21",
        ]
        .iter()
        .map(|d| format!("### {d}\n{filler}\n"))
        .collect::<String>();
        assert!(detect_append_log(&body).is_none());
    }

    #[test]
    fn dated_chunks_carry_entry_dates() {
        // Entries past MIN_LOG_CHUNK_CHARS each fill their own chunk.
        let body = log_body(&["2026-01-01", "2026-02-01", "2026-03-01"], 220);
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
    fn iso_header_rejects_multibyte_at_byte_ten_without_panic() {
        // Date-shaped separators at bytes 4 and 7, but byte 10 lands inside
        // a multibyte scalar — must not panic and must not match.
        assert!(parse_iso_header("2026-01-0é").is_none());
        assert!(parse_header_date("## 2026-01-0é note").is_none());
    }

    #[test]
    fn iso_header_ascii_dates_unchanged() {
        let expect = NaiveDate::from_ymd_opt(2026, 7, 28).unwrap();
        assert_eq!(parse_iso_header("2026-07-28"), Some(expect));
        assert_eq!(parse_iso_header("2026/07/28"), Some(expect));
        // Longer digit run after the day still rejects.
        assert!(parse_iso_header("2026-12-3456").is_none());
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
        let body = log_body(&["2026-01-01", "2026-02-01", "2026-03-01"], 220);
        let log = detect_append_log(&body).unwrap();
        let chunks = chunk_append_log(&body, &log, 2048);
        assert_eq!(chunks.len(), 3);
        assert!(chunks[0].contains("2026-01-01"));
        assert!(chunks[2].contains("2026-03-01"));
    }

    #[test]
    fn short_entries_accumulate_to_the_minimum() {
        // A line a day: 60 entries of ~60 chars accumulate into chunks of at
        // least MIN_LOG_CHUNK_CHARS (bar a short tail merged back), each
        // dated by its newest entry, never splitting an entry.
        let body: String = (1..=60)
            .map(|d| {
                let date = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap() + chrono::Days::new(d);
                format!("{date} walked, read, wrote a little about the day\n")
            })
            .collect();
        let log = detect_append_log(&body).unwrap();
        let chunks = chunk_append_log_dated(&body, &log, 2048);
        assert!(
            chunks.len() > 1 && chunks.len() < 6,
            "{} chunks",
            chunks.len()
        );
        for chunk in &chunks {
            assert!(chunk.content.len() >= MIN_LOG_CHUNK_CHARS);
            assert!(chunk.content.len() <= 2048);
            let newest = chunk.content.lines().last().unwrap()[..10].to_string();
            let want = date_millis(NaiveDate::parse_from_str(&newest, "%Y-%m-%d").unwrap());
            assert_eq!(chunk.entry_date, Some(want), "dated by newest entry");
        }
        let rejoined: Vec<&str> = chunks
            .iter()
            .flat_map(|c| c.content.split("\n\n"))
            .collect();
        assert_eq!(rejoined.len(), 60, "entries stay whole and in order");
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
