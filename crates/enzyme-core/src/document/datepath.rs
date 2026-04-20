use std::path::Path;

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};

/// Month names for parsing (lowercase).
const MONTHS: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];

/// Try to extract a date from the file's relative path.
///
/// Strategies (in priority order):
/// 1. YYYY-MM-DD at start of filename
/// 2. YYYY-MM-DD anywhere in full path
/// 3. Year folder + short date filename (Notion exports): `2026/1 20 abc.md`
/// 4. Month name in filename: `March 15, 2024 standup.md`
/// 5. YYYY/MM/DD path components
/// 6. YYYY/MonthName path components
pub fn extract_date_from_path(rel_path: &Path) -> Option<DateTime<Utc>> {
    let file_stem = rel_path.file_stem()?.to_str()?;
    let full = rel_path.to_str()?;

    // Strategy 1: YYYY-MM-DD at start of filename
    if let Some(dt) = parse_ymd_prefix(file_stem) {
        return Some(dt);
    }

    // Strategy 2: YYYY-MM-DD anywhere in full path
    if let Some(dt) = find_ymd_in_str(full) {
        return Some(dt);
    }

    // Strategy 3: Year folder + short date filename (Notion exports)
    if let Some(dt) = parse_year_folder_short_date(rel_path) {
        return Some(dt);
    }

    // Strategy 4: Month name in filename
    if let Some(dt) = parse_month_name_in_filename(file_stem) {
        return Some(dt);
    }

    // Collect path components for strategies 5 and 6
    let components: Vec<&str> = rel_path.iter().filter_map(|c| c.to_str()).collect();

    // Strategy 5: YYYY/MM/DD path components
    if let Some(dt) = parse_ymd_path_components(&components) {
        return Some(dt);
    }

    // Strategy 6: YYYY/MonthName path components
    if let Some(dt) = parse_year_month_name_components(&components) {
        return Some(dt);
    }

    None
}

/// Parse YYYY-MM-DD at the start of a string. The date may be followed by
/// a space, other characters, or end-of-string.
fn parse_ymd_prefix(s: &str) -> Option<DateTime<Utc>> {
    if s.len() < 10 {
        return None;
    }
    parse_ymd_at(s, 0)
}

/// Find YYYY-MM-DD anywhere in a string.
fn find_ymd_in_str(s: &str) -> Option<DateTime<Utc>> {
    if s.len() < 10 {
        return None;
    }
    // Only try offsets that are valid char boundaries to avoid panicking
    // on multi-byte UTF-8 characters.
    for i in 0..=s.len() - 10 {
        if !s.is_char_boundary(i) {
            continue;
        }
        if let Some(dt) = parse_ymd_at(s, i) {
            return Some(dt);
        }
    }
    None
}

/// Try to parse YYYY-MM-DD at a specific byte offset. Requires the character
/// after the date (if any) to NOT be a digit (avoids matching inside longer numbers).
fn parse_ymd_at(s: &str, offset: usize) -> Option<DateTime<Utc>> {
    let slice = s.get(offset..)?;
    if slice.len() < 10 {
        return None;
    }

    let chunk = slice.get(..10)?;
    // Quick structural check: NNNN-NN-NN
    let b = chunk.as_bytes();
    if b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    if !b[..4].iter().all(|c| c.is_ascii_digit())
        || !b[5..7].iter().all(|c| c.is_ascii_digit())
        || !b[8..10].iter().all(|c| c.is_ascii_digit())
    {
        return None;
    }

    // Reject if preceded by a digit (partial match inside a longer number)
    if offset > 0
        && s.as_bytes()
            .get(offset - 1)
            .is_some_and(|b| b.is_ascii_digit())
    {
        return None;
    }

    // Reject if followed by a digit
    if slice.as_bytes().get(10).is_some_and(|b| b.is_ascii_digit()) {
        return None;
    }

    let year: i32 = chunk[..4].parse().ok()?;
    let month: u32 = chunk[5..7].parse().ok()?;
    let day: u32 = chunk[8..10].parse().ok()?;

    let date = NaiveDate::from_ymd_opt(year, month, day)?;
    Some(naive_date_to_utc(date))
}

/// Strategy 3: parent folder is a 4-digit year, filename starts with `M D` or `M.D`.
/// Matches Notion-style exports like `2026/1 20 abc123.md` or `2026/1.20.md`.
fn parse_year_folder_short_date(rel_path: &Path) -> Option<DateTime<Utc>> {
    let parent = rel_path.parent()?;
    let parent_name = parent.file_name()?.to_str()?;
    let year = parse_four_digit_year(parent_name)?;

    let stem = rel_path.file_stem()?.to_str()?;
    // Try `M D ...` (space-separated) or `M.D...` (dot-separated)
    parse_short_month_day(stem, year)
}

/// Parse `M D ...` or `M.D...` at the start of a string.
fn parse_short_month_day(s: &str, year: i32) -> Option<DateTime<Utc>> {
    // Try space-separated: "1 20 abc" → month=1, day=20
    if let Some(dt) = try_split_month_day(s, ' ', year) {
        return Some(dt);
    }
    // Try dot-separated: "1.20.md" or "1.20" → month=1, day=20
    if let Some(dt) = try_split_month_day(s, '.', year) {
        return Some(dt);
    }
    None
}

/// Split on `sep`, take first two numeric tokens as month and day.
fn try_split_month_day(s: &str, sep: char, year: i32) -> Option<DateTime<Utc>> {
    let mut parts = s.split(sep);
    let month_str = parts.next()?;
    let day_part = parts.next()?;

    let month: u32 = month_str.parse().ok()?;
    // Day part might have trailing non-digit chars when space-separated ("20 abc")
    let day_str = if sep == ' ' {
        day_part
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .unwrap_or(day_part)
    } else {
        day_part
    };
    let day: u32 = day_str.parse().ok()?;

    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    let date = NaiveDate::from_ymd_opt(year, month, day)?;
    Some(naive_date_to_utc(date))
}

/// Strategy 4: month name in filename. Handles:
///   - "March 15, 2024 standup" → 2024-03-15
///   - "January 2024"           → 2024-01-01
fn parse_month_name_in_filename(stem: &str) -> Option<DateTime<Utc>> {
    let lower = stem.to_lowercase();
    for (i, &month_name) in MONTHS.iter().enumerate() {
        if let Some(pos) = lower.find(month_name) {
            let after = &stem[pos + month_name.len()..];
            let month = (i + 1) as u32;
            // Try "Month Day, Year" or "Month Day Year"
            if let Some(dt) = parse_month_day_year(after, month) {
                return Some(dt);
            }
            // Try "Month Year" (day defaults to 1)
            if let Some(dt) = parse_month_year(after, month) {
                return Some(dt);
            }
        }
    }
    None
}

/// Parse ` Day, Year` or ` Day Year` after a month name.
fn parse_month_day_year(after: &str, month: u32) -> Option<DateTime<Utc>> {
    let trimmed = after.trim_start();
    // Extract day digits
    let day_end = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(trimmed.len());
    if day_end == 0 {
        return None;
    }
    let day: u32 = trimmed[..day_end].parse().ok()?;
    if !(1..=31).contains(&day) {
        return None;
    }

    // Skip optional comma and whitespace
    let rest = trimmed[day_end..].trim_start_matches(',').trim_start();
    // Extract year
    let year_end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    if year_end < 4 {
        return None;
    }
    let year: i32 = rest[..year_end].parse().ok()?;
    if !(1900..=2100).contains(&year) {
        return None;
    }

    let date = NaiveDate::from_ymd_opt(year, month, day)?;
    Some(naive_date_to_utc(date))
}

/// Parse ` Year` after a month name (day defaults to 1).
fn parse_month_year(after: &str, month: u32) -> Option<DateTime<Utc>> {
    let trimmed = after.trim_start();
    let year_end = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(trimmed.len());
    if year_end < 4 {
        return None;
    }
    let year: i32 = trimmed[..year_end].parse().ok()?;
    if !(1900..=2100).contains(&year) {
        return None;
    }

    let date = NaiveDate::from_ymd_opt(year, month, 1)?;
    Some(naive_date_to_utc(date))
}

/// Strategy 5: YYYY/MM/DD as consecutive path components.
fn parse_ymd_path_components(components: &[&str]) -> Option<DateTime<Utc>> {
    if components.len() < 3 {
        return None;
    }
    for window in components.windows(3) {
        let year = parse_four_digit_year(window[0])?;
        let month: u32 = window[1].parse().ok()?;
        let day: u32 = window[2].parse().ok()?;
        if (1..=12).contains(&month)
            && (1..=31).contains(&day)
            && let Some(date) = NaiveDate::from_ymd_opt(year, month, day)
        {
            return Some(naive_date_to_utc(date));
        }
    }
    None
}

/// Strategy 6: YYYY/MonthName as consecutive path components (day defaults to 1).
fn parse_year_month_name_components(components: &[&str]) -> Option<DateTime<Utc>> {
    if components.len() < 2 {
        return None;
    }
    for window in components.windows(2) {
        if let Some(year) = parse_four_digit_year(window[0]) {
            let lower = window[1].to_lowercase();
            for (i, &month_name) in MONTHS.iter().enumerate() {
                if lower == month_name {
                    let month = (i + 1) as u32;
                    if let Some(date) = NaiveDate::from_ymd_opt(year, month, 1) {
                        return Some(naive_date_to_utc(date));
                    }
                }
            }
        }
    }
    None
}

/// Parse a string as exactly a 4-digit year in a reasonable range.
fn parse_four_digit_year(s: &str) -> Option<i32> {
    if s.len() != 4 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let year: i32 = s.parse().ok()?;
    if (1900..=2100).contains(&year) {
        Some(year)
    } else {
        None
    }
}

fn naive_date_to_utc(date: NaiveDate) -> DateTime<Utc> {
    let naive_dt = NaiveDateTime::new(date, NaiveTime::MIN);
    Utc.from_utc_datetime(&naive_dt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn dt(y: i32, m: u32, d: u32) -> DateTime<Utc> {
        let date = NaiveDate::from_ymd_opt(y, m, d).unwrap();
        naive_date_to_utc(date)
    }

    // --- Strategy 1: YYYY-MM-DD at start of filename ---

    #[test]
    fn ymd_start_of_filename() {
        let p = PathBuf::from("daily/2024-01-15.md");
        assert_eq!(extract_date_from_path(&p), Some(dt(2024, 1, 15)));
    }

    #[test]
    fn ymd_start_with_suffix() {
        let p = PathBuf::from("2024-01-15 Meeting Notes.md");
        assert_eq!(extract_date_from_path(&p), Some(dt(2024, 1, 15)));
    }

    // --- Strategy 2: YYYY-MM-DD anywhere in path ---

    #[test]
    fn ymd_in_folder() {
        let p = PathBuf::from("daily/2024-01-15/note.md");
        assert_eq!(extract_date_from_path(&p), Some(dt(2024, 1, 15)));
    }

    #[test]
    fn ymd_mid_filename() {
        let p = PathBuf::from("meeting-2024-01-15-notes.md");
        assert_eq!(extract_date_from_path(&p), Some(dt(2024, 1, 15)));
    }

    // --- Strategy 3: Year folder + short date (Notion exports) ---

    #[test]
    fn notion_space_separated() {
        let p = PathBuf::from("2026/1 20 abc123.md");
        assert_eq!(extract_date_from_path(&p), Some(dt(2026, 1, 20)));
    }

    #[test]
    fn notion_dot_separated() {
        let p = PathBuf::from("2026/1.20.md");
        assert_eq!(extract_date_from_path(&p), Some(dt(2026, 1, 20)));
    }

    #[test]
    fn notion_single_digit_day() {
        let p = PathBuf::from("2026/2.3.md");
        assert_eq!(extract_date_from_path(&p), Some(dt(2026, 2, 3)));
    }

    // --- Strategy 4: Month name in filename ---

    #[test]
    fn month_day_year() {
        let p = PathBuf::from("notes/March 15, 2024 standup.md");
        assert_eq!(extract_date_from_path(&p), Some(dt(2024, 3, 15)));
    }

    #[test]
    fn month_year_only() {
        let p = PathBuf::from("January 2024.md");
        assert_eq!(extract_date_from_path(&p), Some(dt(2024, 1, 1)));
    }

    #[test]
    fn month_day_year_no_comma() {
        let p = PathBuf::from("notes/March 15 2024.md");
        assert_eq!(extract_date_from_path(&p), Some(dt(2024, 3, 15)));
    }

    // --- Strategy 5: YYYY/MM/DD path components ---

    #[test]
    fn ymd_path_components() {
        let p = PathBuf::from("2024/01/15/note.md");
        assert_eq!(extract_date_from_path(&p), Some(dt(2024, 1, 15)));
    }

    // --- Strategy 6: YYYY/MonthName path components ---

    #[test]
    fn year_month_name_components() {
        let p = PathBuf::from("2024/January/note.md");
        assert_eq!(extract_date_from_path(&p), Some(dt(2024, 1, 1)));
    }

    // --- Edge cases ---

    #[test]
    fn no_date_in_path() {
        let p = PathBuf::from("notes/random-note.md");
        assert_eq!(extract_date_from_path(&p), None);
    }

    #[test]
    fn invalid_date_rejected() {
        let p = PathBuf::from("2024-13-40.md");
        assert_eq!(extract_date_from_path(&p), None);
    }

    #[test]
    fn short_filename_no_panic() {
        let p = PathBuf::from("hi.md");
        assert_eq!(extract_date_from_path(&p), None);
    }

    #[test]
    fn multibyte_chars_no_panic() {
        // Filenames with multi-byte UTF-8 characters that caused panics
        let cases = [
            "Primer on \u{201c}Tending a Quiet Mind\u{201d} by Ada Fenwick Highlights.md",
            "d Writing\u{2014}And Us by Nell Okafor Highlights.md",
            "025 TALK \u{00b7} GitHub by 262588213843476 Highlights.md",
            "A.I. Isn\u{2019}t Going to Paint the Harbor Review by Oren Vale Highlights.md",
            "Goes Up \u{2013} The Problem With NFTs by Folding Ideas Highlights.md",
            "Louis Ferr\u{00e9}e Highlights.md",
            "t by Bren\u{00e9} Brown Highlights.md",
        ];
        for name in &cases {
            let p = PathBuf::from(name);
            // Should not panic, result doesn't matter
            let _ = extract_date_from_path(&p);
        }
    }
}
