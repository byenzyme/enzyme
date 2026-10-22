//! Source-declared date rules: `date notes in "Journal" by filename as "YYMMDD"`.
//!
//! A Markdown source can say where its dates live and how they are written.
//! Rules are tried before the built-in date parsing, which stays the fallback
//! unless a rule in scope says `only`. Rules are scoped by folder; the most
//! specific folder's rules are tried first, then its parents', in written
//! order within one folder.

use std::collections::HashMap;
use std::path::Path;

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use enzyme_spec::{DateFormat, DatePlace, DateRule, DateTarget, DateToken};
use serde_yaml::Value;
use tracing::warn;

use super::appendlog::{header_text, parse_builtin_header_date};
use super::datepath::{extract_date_from_path, month_from_token};
use super::frontmatter::value_to_datetime;
use crate::models::DateSource;

/// A source's compiled date rules.
#[derive(Debug, Clone, Default)]
pub struct DateRules {
    rules: Vec<CompiledRule>,
    fingerprint: String,
}

#[derive(Debug, Clone)]
struct CompiledRule {
    target: DateTarget,
    /// Lowercased folder components; empty covers the whole source.
    folder: Vec<String>,
    place: DatePlace,
    formats: Vec<Format>,
    only: bool,
}

#[derive(Debug, Clone)]
enum Format {
    Pattern(Vec<DateToken>),
    UnixSeconds,
    UnixMilliseconds,
}

impl DateRules {
    pub fn new(rules: &[DateRule]) -> Self {
        let compiled = rules
            .iter()
            .filter_map(|rule| {
                let formats = rule
                    .formats
                    .iter()
                    .map(|format| match format {
                        DateFormat::Pattern(pattern) => {
                            enzyme_spec::date_pattern(pattern).map(Format::Pattern)
                        }
                        DateFormat::UnixSeconds => Ok(Format::UnixSeconds),
                        DateFormat::UnixMilliseconds => Ok(Format::UnixMilliseconds),
                    })
                    .collect::<anyhow::Result<Vec<_>>>();
                // The parser validates patterns; a stale or hand-edited
                // compiled config must not fail indexing.
                let formats = match formats {
                    Ok(formats) => formats,
                    Err(error) => {
                        warn!(%error, "skipping date rule with an invalid format");
                        return None;
                    }
                };
                Some(CompiledRule {
                    target: rule.target,
                    folder: folder_components(rule.folder.as_deref().unwrap_or("")),
                    place: rule.place.clone(),
                    formats,
                    only: rule.only,
                })
            })
            .collect();
        let fingerprint = if rules.is_empty() {
            String::new()
        } else {
            serde_json::to_string(rules).unwrap_or_default()
        };
        Self {
            rules: compiled,
            fingerprint,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Stable identity of the declared rules; empty when there are none. A
    /// change means stored dates may be stale.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// The rules covering one document, most specific folder first.
    pub fn for_path(&self, rel_path: &Path) -> DocumentDates<'_> {
        let parent = rel_path
            .parent()
            .map(|p| folder_components(&p.to_string_lossy()))
            .unwrap_or_default();
        let mut applicable: Vec<&CompiledRule> = self
            .rules
            .iter()
            .filter(|rule| parent.starts_with(&rule.folder))
            .collect();
        // Stable: written order survives within one folder depth.
        applicable.sort_by_key(|rule| std::cmp::Reverse(rule.folder.len()));
        let of = |target| -> Vec<&CompiledRule> {
            applicable
                .iter()
                .copied()
                .filter(|rule| rule.target == target)
                .collect()
        };
        DocumentDates {
            notes: of(DateTarget::Notes),
            entries: of(DateTarget::Entries),
            edits: of(DateTarget::Edits),
        }
    }
}

/// The date rules that apply to one document.
#[derive(Debug, Clone, Default)]
pub struct DocumentDates<'a> {
    notes: Vec<&'a CompiledRule>,
    entries: Vec<&'a CompiledRule>,
    edits: Vec<&'a CompiledRule>,
}

impl DocumentDates<'_> {
    /// Whether any rule reads a frontmatter field (so the raw mapping is needed).
    pub fn reads_fields(&self) -> bool {
        self.notes
            .iter()
            .chain(&self.edits)
            .any(|rule| matches!(rule.place, DatePlace::Field { .. }))
    }

    pub fn has_entry_rules(&self) -> bool {
        !self.entries.is_empty()
    }

    /// Built-in created-date parsing (frontmatter keys, path, content) stays on.
    pub fn builtin_created(&self) -> bool {
        !self.notes.iter().any(|rule| rule.only)
    }

    /// Built-in modified-date parsing stays on.
    pub fn builtin_modified(&self) -> bool {
        !self.edits.iter().any(|rule| rule.only)
    }

    /// The created date from the first rule that finds one.
    pub fn created(
        &self,
        frontmatter: Option<&HashMap<String, Value>>,
        rel_path: &Path,
    ) -> Option<(DateTime<Utc>, DateSource)> {
        self.notes.iter().find_map(|rule| match &rule.place {
            DatePlace::Field { names } => {
                field_date(frontmatter?, names, &rule.formats).map(|d| (d, DateSource::Frontmatter))
            }
            DatePlace::Filename => {
                let stem = rel_path.file_stem()?.to_str()?;
                let date = if rule.formats.is_empty() {
                    extract_date_from_path(Path::new(stem))
                } else {
                    search_formats(&rule.formats, stem)
                };
                date.map(|d| (d, DateSource::Filename))
            }
            DatePlace::Path => {
                let date = if rule.formats.is_empty() {
                    extract_date_from_path(rel_path)
                } else {
                    let text = rel_path.with_extension("");
                    search_formats(&rule.formats, &text.to_string_lossy().replace('\\', "/"))
                };
                date.map(|d| (d, DateSource::Filename))
            }
            DatePlace::Headings => None,
        })
    }

    /// The modified date from the first field rule that finds one.
    pub fn modified(&self, frontmatter: Option<&HashMap<String, Value>>) -> Option<DateTime<Utc>> {
        let frontmatter = frontmatter?;
        self.edits.iter().find_map(|rule| match &rule.place {
            DatePlace::Field { names } => field_date(frontmatter, names, &rule.formats),
            _ => None,
        })
    }

    /// Built-in log-entry date parsing stays on.
    pub fn builtin_entries(&self) -> bool {
        !self.entries.iter().any(|rule| rule.only)
    }

    /// Read a log entry date from a header-ish line: declared heading formats
    /// first, then the built-in forms unless a rule says `only`.
    pub fn header_date(&self, line: &str) -> Option<NaiveDate> {
        self.rule_header_date(line).or_else(|| {
            if self.builtin_entries() {
                parse_builtin_header_date(header_text(line))
            } else {
                None
            }
        })
    }

    /// The declared heading formats alone, without the built-in fallback.
    /// Used where the caller has its own built-in readers (declared logs).
    pub fn rule_header_date(&self, line: &str) -> Option<NaiveDate> {
        let text = header_text(line);
        for rule in &self.entries {
            if rule.formats.is_empty() {
                if let Some(date) = parse_builtin_header_date(text) {
                    return Some(date);
                }
            }
            for format in &rule.formats {
                if let Format::Pattern(tokens) = format {
                    if let Some((date, used)) = match_tokens(tokens, text) {
                        if !text[used..].starts_with(|c: char| c.is_ascii_digit()) {
                            return Some(date);
                        }
                    }
                }
            }
        }
        None
    }
}

fn folder_components(folder: &str) -> Vec<String> {
    folder
        .split(['/', '\\'])
        .map(str::trim)
        .filter(|part| !part.is_empty() && *part != ".")
        .map(str::to_lowercase)
        .collect()
}

/// The first named field (case-insensitive) whose value parses.
fn field_date(
    frontmatter: &HashMap<String, Value>,
    names: &[String],
    formats: &[Format],
) -> Option<DateTime<Utc>> {
    names.iter().find_map(|name| {
        let wanted = name.trim().to_lowercase();
        let value = frontmatter
            .iter()
            .find(|(key, _)| key.trim().to_lowercase() == wanted)
            .map(|(_, value)| value)?;
        value_date(value, formats)
    })
}

fn value_date(value: &Value, formats: &[Format]) -> Option<DateTime<Utc>> {
    if let Value::Sequence(items) = value {
        return items.iter().find_map(|item| value_date(item, formats));
    }
    if formats.is_empty() {
        return value_to_datetime(value);
    }
    formats.iter().find_map(|format| match format {
        Format::Pattern(tokens) => {
            let text = match value {
                Value::String(s) => {
                    let s = s.trim();
                    s.strip_prefix("[[")
                        .and_then(|s| s.strip_suffix("]]"))
                        .unwrap_or(s)
                        .trim()
                        .to_string()
                }
                // A bare compact date (`created: 250314`) is a YAML number;
                // read its digits, restoring leading zeros YAML dropped.
                Value::Number(n) => {
                    let digits = n.as_u64()?.to_string();
                    match fixed_digit_width(tokens) {
                        Some(width) if digits.len() < width => {
                            format!("{digits:0>width$}")
                        }
                        _ => digits,
                    }
                }
                _ => return None,
            };
            let (date, used) = match_tokens(tokens, &text)?;
            (used == text.len()).then(|| to_utc(date))
        }
        Format::UnixSeconds => DateTime::from_timestamp(number(value)?, 0),
        Format::UnixMilliseconds => DateTime::from_timestamp_millis(number(value)?),
    })
}

fn number(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Total width of a pattern made only of fixed-width digit tokens.
fn fixed_digit_width(tokens: &[DateToken]) -> Option<usize> {
    tokens
        .iter()
        .map(|token| match token {
            DateToken::Year4 => Some(4),
            DateToken::Year2 | DateToken::Month2 | DateToken::Day2 => Some(2),
            _ => None,
        })
        .sum()
}

/// Find a date anywhere in `text` that starts at a word boundary and is not
/// followed by another digit.
fn search_formats(formats: &[Format], text: &str) -> Option<DateTime<Utc>> {
    formats.iter().find_map(|format| {
        let Format::Pattern(tokens) = format else {
            return None;
        };
        text.char_indices().find_map(|(i, _)| {
            let boundary = text[..i]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_alphanumeric());
            if !boundary {
                return None;
            }
            let rest = &text[i..];
            let (date, used) = match_tokens(tokens, rest)?;
            (!rest[used..].starts_with(|c: char| c.is_ascii_digit())).then(|| to_utc(date))
        })
    })
}

/// Match a compiled pattern at the start of `s`, returning the date and the
/// number of bytes consumed.
fn match_tokens(tokens: &[DateToken], s: &str) -> Option<(NaiveDate, usize)> {
    let (mut year, mut month, mut day) = (None, None, 1u32);
    let mut pos = 0usize;
    for token in tokens {
        let rest = &s[pos..];
        match token {
            DateToken::Year4 => {
                year = Some(digits(rest, 4, 4)?.0 as i32);
                pos += 4;
            }
            DateToken::Year2 => {
                // POSIX `%y` pivot: 69–99 are the 1900s, 00–68 the 2000s.
                let yy = digits(rest, 2, 2)?.0 as i32;
                year = Some(if yy >= 69 { 1900 + yy } else { 2000 + yy });
                pos += 2;
            }
            DateToken::Month2 | DateToken::Month | DateToken::Day2 | DateToken::Day => {
                let min = if matches!(token, DateToken::Month2 | DateToken::Day2) {
                    2
                } else {
                    1
                };
                let (value, width) = digits(rest, min, 2)?;
                if matches!(token, DateToken::Month2 | DateToken::Month) {
                    month = Some(value);
                } else {
                    day = value;
                }
                pos += width;
            }
            DateToken::MonthName => {
                let width = rest
                    .find(|c: char| !c.is_ascii_alphabetic() && c != '.')
                    .unwrap_or(rest.len());
                month = Some(month_from_token(&rest[..width])?);
                pos += width;
            }
            DateToken::Literal(c) if c.is_whitespace() => {
                let width = rest.len() - rest.trim_start().len();
                if width == 0 {
                    return None;
                }
                pos += width;
            }
            DateToken::Literal(c) => {
                if !rest.starts_with(*c) {
                    return None;
                }
                pos += c.len_utf8();
            }
        }
    }
    let year = year?;
    if !(1900..=2100).contains(&year) {
        return None;
    }
    Some((NaiveDate::from_ymd_opt(year, month?, day)?, pos))
}

/// A run of `min..=max` ASCII digits at the start of `s`: (value, width).
fn digits(s: &str, min: usize, max: usize) -> Option<(u32, usize)> {
    let width = s
        .bytes()
        .take(max)
        .take_while(|b| b.is_ascii_digit())
        .count();
    if width < min {
        return None;
    }
    Some((s[..width].parse().ok()?, width))
}

fn to_utc(date: NaiveDate) -> DateTime<Utc> {
    Utc.from_utc_datetime(&NaiveDateTime::new(date, NaiveTime::MIN))
}

#[cfg(test)]
mod tests {
    use super::*;
    use enzyme_spec::parse;

    fn compile_rules(program: &str) -> DateRules {
        let parsed = parse(&format!("vault \"/notes\" {{\n{program}\n}}")).unwrap();
        DateRules::new(&parsed.vaults[0].date_rules)
    }

    fn day(y: i32, m: u32, d: u32) -> DateTime<Utc> {
        to_utc(NaiveDate::from_ymd_opt(y, m, d).unwrap())
    }

    fn yaml(source: &str) -> HashMap<String, Value> {
        serde_yaml::from_str(source).unwrap()
    }

    #[test]
    fn compact_filename_dates_read_in_their_folder_only() {
        let rules = compile_rules(r#"date notes in "Journal" by filename as "YYMMDD""#);
        assert_eq!(
            rules
                .for_path(Path::new("Journal/2025/250314 standup.md"))
                .created(None, Path::new("Journal/2025/250314 standup.md")),
            Some((day(2025, 3, 14), DateSource::Filename))
        );
        let outside = Path::new("Inbox/250314 standup.md");
        assert_eq!(rules.for_path(outside).created(None, outside), None);
        // Seven digits is not a six-digit date.
        let longer = Path::new("Journal/2503140 id.md");
        assert_eq!(rules.for_path(longer).created(None, longer), None);
    }

    #[test]
    fn folder_scope_is_case_insensitive_and_most_specific_wins() {
        let rules = compile_rules(
            r#"date notes by filename as "DDMMYY"
               date notes in "journal" by filename as "YYMMDD""#,
        );
        let path = Path::new("Journal/250314.md");
        assert_eq!(
            rules.for_path(path).created(None, path),
            Some((day(2025, 3, 14), DateSource::Filename))
        );
        let elsewhere = Path::new("Inbox/250314.md");
        assert_eq!(
            rules.for_path(elsewhere).created(None, elsewhere),
            Some((day(2014, 3, 25), DateSource::Filename))
        );
    }

    #[test]
    fn field_rules_read_numbers_strings_lists_and_unix_time() {
        let rules = compile_rules(
            r#"date notes by field "day" or "created" as "YYMMDD"
               date notes in "Imports" by field "ts" as unix seconds"#,
        );
        let path = Path::new("note.md");
        let dates = rules.for_path(path);
        assert!(dates.reads_fields());
        for source in [
            "created: 250314",
            "created: \"250314\"",
            "Created: '[[250314]]'",
            "day: [garbage, 250314]",
        ] {
            assert_eq!(
                dates.created(Some(&yaml(source)), path),
                Some((day(2025, 3, 14), DateSource::Frontmatter)),
                "{source}"
            );
        }
        // YAML drops the leading zero of 050314; the pattern width restores it.
        assert_eq!(
            dates.created(Some(&yaml("created: 050314")), path),
            Some((day(2005, 3, 14), DateSource::Frontmatter))
        );
        assert_eq!(
            dates.created(Some(&yaml("created: 2025-03-14")), path),
            None
        );
        let imported = Path::new("Imports/a.md");
        assert_eq!(
            rules
                .for_path(imported)
                .created(Some(&yaml("ts: 1741910400")), imported),
            Some((day(2025, 3, 14), DateSource::Frontmatter))
        );
    }

    #[test]
    fn field_rule_without_format_uses_builtin_formats() {
        let rules = compile_rules(r#"date edits by field "reviewed""#);
        let dates = rules.for_path(Path::new("a.md"));
        assert_eq!(
            dates.modified(Some(&yaml("reviewed: 2024-01-15"))),
            Some(day(2024, 1, 15))
        );
        assert!(dates.builtin_modified());
    }

    #[test]
    fn only_turns_off_builtin_fallbacks_in_scope() {
        let rules = compile_rules(r#"date notes in "Zettel" by field "created" only"#);
        assert!(!rules.for_path(Path::new("Zettel/a.md")).builtin_created());
        assert!(rules.for_path(Path::new("Other/a.md")).builtin_created());
    }

    #[test]
    fn heading_rules_add_formats_ahead_of_builtins() {
        let rules = compile_rules(r#"date entries in "Journal" by headings as "YYMMDD""#);
        let dates = rules.for_path(Path::new("Journal/reading log.md"));
        assert_eq!(
            dates.header_date("## 250314 planning\n"),
            NaiveDate::from_ymd_opt(2025, 3, 14)
        );
        assert_eq!(
            dates.header_date("### Fri 250314\n"),
            NaiveDate::from_ymd_opt(2025, 3, 14)
        );
        // Built-ins still apply without `only`.
        assert_eq!(
            dates.header_date("## 2025-03-15\n"),
            NaiveDate::from_ymd_opt(2025, 3, 15)
        );
        assert_eq!(dates.header_date("## 2503141\n"), None);
        assert_eq!(dates.header_date("Met on 250314\n"), None);

        let strict = compile_rules(r#"date entries by headings as "YYMMDD" only"#);
        let strict = strict.for_path(Path::new("Journal/x.md"));
        assert_eq!(strict.header_date("## 2025-03-15\n"), None);
    }

    #[test]
    fn rule_header_dates_leave_the_builtins_to_the_caller() {
        let rules = compile_rules(r#"date entries by headings as "DD.MM.YY""#);
        let dates = rules.for_path(Path::new("log.md"));
        assert_eq!(
            dates.rule_header_date("## 14.03.25\n"),
            NaiveDate::from_ymd_opt(2025, 3, 14)
        );
        assert_eq!(dates.rule_header_date("## 2025-03-15\n"), None);
        assert!(dates.builtin_entries());
        let strict = compile_rules(r#"date entries by headings as "DD.MM.YY" only"#);
        assert!(!strict.for_path(Path::new("log.md")).builtin_entries());
    }

    #[test]
    fn path_rules_search_folder_names() {
        let rules = compile_rules(r#"date notes by path as "YYYY/MM/DD" or "YYYY.MM""#);
        let path = Path::new("archive/2025/03/14/standup.md");
        assert_eq!(
            rules.for_path(path).created(None, path),
            Some((day(2025, 3, 14), DateSource::Filename))
        );
        let month = Path::new("logs/2023.11/notes.md");
        assert_eq!(
            rules.for_path(month).created(None, month),
            Some((day(2023, 11, 1), DateSource::Filename))
        );
    }

    #[test]
    fn month_names_and_variable_width_tokens_match() {
        let rules = compile_rules(r#"date notes by filename as "D MMM YYYY""#);
        let path = Path::new("14 Mar. 2025 offsite.md");
        assert_eq!(
            rules.for_path(path).created(None, path),
            Some((day(2025, 3, 14), DateSource::Filename))
        );
    }

    #[test]
    fn fingerprint_tracks_declared_rules() {
        assert_eq!(DateRules::default().fingerprint(), "");
        let a = compile_rules(r#"date notes by filename as "YYMMDD""#);
        let b = compile_rules(r#"date notes by filename as "DDMMYY""#);
        assert!(!a.fingerprint().is_empty());
        assert_ne!(a.fingerprint(), b.fingerprint());
    }
}
