use std::collections::{HashMap, HashSet};

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use serde_yaml::Value;

/// Parsed YAML frontmatter from a markdown document.
#[derive(Debug, Clone, Default)]
pub struct Frontmatter {
    pub title: Option<String>,
    pub tags: Vec<String>,
    pub created: Option<DateTime<Utc>>,
    pub modified: Option<DateTime<Utc>>,
    pub aliases: Vec<String>,
    /// Wikilinks extracted from any frontmatter field (people, author, related, etc.).
    /// Normalized to lowercase, target only (no aliases or headings).
    pub links: Vec<String>,
    /// All other key-value pairs not captured by named fields.
    pub extra: HashMap<String, Value>,
}

impl Frontmatter {
    /// Promote plain string values from configured frontmatter fields into
    /// generic link entities.
    ///
    /// Literal wikilinks are already extracted from every field without
    /// configuration. This method only handles unbracketed scalar strings and
    /// string lists from deliberately selected fields such as `people` or
    /// `projects`. Field names are matched case-insensitively.
    pub fn promote_link_fields(&mut self, fields: &HashSet<String>) {
        if fields.is_empty() {
            return;
        }

        let normalized_fields: HashSet<String> = fields
            .iter()
            .map(|field| field.trim().to_lowercase())
            .filter(|field| !field.is_empty())
            .collect();
        let mut promoted: HashSet<String> = HashSet::new();
        for (key, value) in &self.extra {
            if normalized_fields.contains(&key.trim().to_lowercase()) {
                collect_plain_link_values(value, &mut promoted);
            }
        }
        self.links.extend(promoted);
        self.links.sort();
        self.links.dedup();
    }
}

/// Parse YAML frontmatter delimited by `---` at the start of a document.
///
/// Returns `None` if the document has no valid frontmatter block.
pub fn parse_frontmatter(content: &str) -> Option<Frontmatter> {
    let mapping = frontmatter_mapping(content)?;
    if mapping.is_empty() {
        return Some(Frontmatter::default());
    }
    Some(frontmatter_from_mapping(mapping))
}

/// The raw YAML mapping of a document's frontmatter block, for readers that
/// need fields the typed [`Frontmatter`] consumes (configured date fields).
/// An empty block yields an empty mapping; no block or invalid YAML, `None`.
pub(crate) fn frontmatter_mapping(content: &str) -> Option<HashMap<String, Value>> {
    let trimmed = content.trim_start();

    // Must start with `---` on its own line.
    if !trimmed.starts_with("---") {
        return None;
    }

    // Find the closing `---`.
    let after_opening = &trimmed[3..];
    // Skip the rest of the opening line (there might be nothing, or whitespace).
    let after_first_newline = after_opening.find('\n').map(|i| &after_opening[i + 1..])?;

    // Handle empty frontmatter: closing `---` is immediately after the opening.
    if after_first_newline.starts_with("---") {
        return Some(HashMap::new());
    }

    let closing = after_first_newline.find("\n---")?;
    let yaml_str = &after_first_newline[..closing];

    if yaml_str.trim().is_empty() {
        return Some(HashMap::new());
    }

    serde_yaml::from_str(yaml_str).ok()
}

fn frontmatter_from_mapping(mapping: HashMap<String, Value>) -> Frontmatter {
    let mut fm = Frontmatter::default();

    // Title
    if let Some(val) = mapping.get("title") {
        fm.title = value_to_string(val);
    }

    // Tags — support multiple formats
    if let Some(val) = mapping.get("tags") {
        fm.tags = value_to_string_vec(val);
    }
    // Also accept singular `tag`
    if let Some(val) = mapping.get("tag") {
        let extra_tags = value_to_string_vec(val);
        for t in extra_tags {
            if !fm.tags.contains(&t) {
                fm.tags.push(t);
            }
        }
    }
    // Normalize tags to lowercase and strip leading `#`.
    fm.tags = fm
        .tags
        .iter()
        .map(|t| {
            let stripped = t.strip_prefix('#').unwrap_or(t);
            stripped.to_lowercase()
        })
        .collect();

    // Dates — try field name aliases in priority order
    static CREATED_FIELDS: &[&str] = &[
        "created",
        "date",
        "date_created",
        "date created",
        "createdAt",
        "created_at",
    ];
    static MODIFIED_FIELDS: &[&str] = &[
        "modified",
        "updated",
        "date_modified",
        "date modified",
        "lastModified",
        "last_modified",
        "updatedAt",
    ];

    for &key in CREATED_FIELDS {
        if fm.created.is_some() {
            break;
        }
        if let Some(val) = mapping.get(key) {
            fm.created = value_to_datetime(val);
        }
    }
    for &key in MODIFIED_FIELDS {
        if fm.modified.is_some() {
            break;
        }
        if let Some(val) = mapping.get(key) {
            fm.modified = value_to_datetime(val);
        }
    }

    // Aliases
    if let Some(val) = mapping.get("aliases") {
        fm.aliases = value_to_string_vec(val);
    }

    // Extra — everything else
    let known_keys = [
        "title",
        "tags",
        "tag",
        "created",
        "modified",
        "updated",
        "date",
        "createdAt",
        "created_at",
        "date created",
        "lastModified",
        "last_modified",
        "updatedAt",
        "date modified",
        "date_created",
        "date_modified",
        "aliases",
    ];
    for (key, val) in &mapping {
        if !known_keys.contains(&key.as_str()) {
            fm.extra.insert(key.clone(), val.clone());
        }
    }

    // Extract wikilinks from all frontmatter fields (people, author, related, etc.)
    fm.links = extract_frontmatter_wikilinks(&mapping);

    fm
}

/// Extract the body of a document (everything after frontmatter).
pub fn strip_frontmatter(content: &str) -> &str {
    let trimmed = content.trim_start();

    if !trimmed.starts_with("---") {
        return content;
    }

    let after_opening = &trimmed[3..];
    let Some(after_first_newline_idx) = after_opening.find('\n') else {
        return content;
    };
    let after_first_newline = &after_opening[after_first_newline_idx + 1..];

    // Handle empty frontmatter: closing `---` immediately after opening.
    if after_first_newline.starts_with("---") {
        let rest = &after_first_newline[3..];
        return match rest.find('\n') {
            Some(nl) => &rest[nl + 1..],
            None => "",
        };
    }

    match after_first_newline.find("\n---") {
        Some(closing) => {
            let rest = &after_first_newline[closing + 4..]; // skip "\n---"
            // Skip the remainder of the closing `---` line
            match rest.find('\n') {
                Some(nl) => &rest[nl + 1..],
                None => "", // nothing after frontmatter
            }
        }
        None => content,
    }
}

/// Extract wikilinks from all frontmatter field values.
///
/// Scans every field's string representation for `[[Target]]`, `[[Target|Alias]]`,
/// and `[[Target#Heading]]` patterns. Returns deduplicated, lowercase targets.
///
/// This captures links from fields like `people: [[Alice]], [[Bob]]`,
/// `author: [[Charlie]]`, `related: [[Project Alpha]]`, etc.
fn extract_frontmatter_wikilinks(mapping: &HashMap<String, Value>) -> Vec<String> {
    let mut links: HashSet<String> = HashSet::new();

    for val in mapping.values() {
        collect_wikilinks_from_value(val, &mut links);
    }

    let mut result: Vec<String> = links.into_iter().collect();
    result.sort();
    result
}

/// Recursively collect wikilinks from a YAML value (handles strings, sequences, mappings).
fn collect_wikilinks_from_value(val: &Value, links: &mut HashSet<String>) {
    match val {
        Value::String(s) => {
            extract_wikilinks_from_str(s, links);
        }
        Value::Sequence(seq) => {
            for item in seq {
                collect_wikilinks_from_value(item, links);
            }
        }
        Value::Mapping(map) => {
            for v in map.values() {
                collect_wikilinks_from_value(v, links);
            }
        }
        _ => {}
    }
}

/// Collect plain entity handles from a configured field value.
///
/// Only strings and nested string sequences are accepted. Mappings, numbers,
/// booleans, and nulls are ignored so a broad metadata object cannot silently
/// turn roles, IDs, or flags into entities. Strings containing wikilink syntax
/// are also ignored here because the normal wikilink extractor already emits
/// their exact targets.
fn collect_plain_link_values(val: &Value, links: &mut HashSet<String>) {
    match val {
        Value::String(value) if !value.contains("[[") => {
            let normalized = value.trim().to_lowercase();
            if !normalized.is_empty() {
                links.insert(normalized);
            }
        }
        Value::Sequence(values) => {
            for value in values {
                collect_plain_link_values(value, links);
            }
        }
        _ => {}
    }
}

/// Extract `[[target]]` patterns from a string, normalized to lowercase.
fn extract_wikilinks_from_str(s: &str, links: &mut HashSet<String>) {
    let mut search_from = 0;
    let bytes = s.as_bytes();

    while search_from + 3 < bytes.len() {
        let Some(open) = s[search_from..].find("[[") else {
            break;
        };
        let abs_open = search_from + open + 2;

        let Some(close) = s[abs_open..].find("]]") else {
            break;
        };
        let abs_close = abs_open + close;

        let inner = &s[abs_open..abs_close];

        if !inner.contains('\n') && !inner.is_empty() {
            // Take target before | (alias) or # (heading)
            let target = inner
                .split('|')
                .next()
                .unwrap_or(inner)
                .split('#')
                .next()
                .unwrap_or(inner)
                .trim();
            if !target.is_empty() {
                links.insert(target.to_lowercase());
            }
        }

        search_from = abs_close + 2;
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn value_to_string(val: &Value) -> Option<String> {
    match val {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn value_to_string_vec(val: &Value) -> Vec<String> {
    match val {
        Value::Sequence(seq) => seq.iter().filter_map(value_to_string).collect(),
        Value::String(s) => {
            // Could be comma-separated: "a, b, c"
            if s.contains(',') {
                s.split(',').map(|p| p.trim().to_string()).collect()
            } else {
                vec![s.clone()]
            }
        }
        Value::Number(n) => vec![n.to_string()],
        _ => Vec::new(),
    }
}

/// Smallest magnitude read as a Unix timestamp in seconds (1973-03-03). A
/// shorter bare number is a compact date or an ID, not epoch time: reading
/// `created: 250314` as seconds would date the note to January 1970.
const MIN_UNIX_SECONDS: i64 = 100_000_000;

pub(crate) fn value_to_datetime(val: &Value) -> Option<DateTime<Utc>> {
    let s = match val {
        Value::String(s) => {
            // Strip wikilink brackets: "[[2025-08-04]]" -> "2025-08-04"
            let stripped = s.trim();
            if stripped.starts_with("[[") && stripped.ends_with("]]") {
                stripped[2..stripped.len() - 2].to_string()
            } else {
                s.clone()
            }
        }
        Value::Number(n) => {
            // Could be a unix timestamp
            if let Some(ts) = n.as_i64() {
                return (ts.abs() >= MIN_UNIX_SECONDS)
                    .then(|| DateTime::from_timestamp(ts, 0))
                    .flatten();
            }
            if let Some(ts) = n.as_f64() {
                return (ts.abs() >= MIN_UNIX_SECONDS as f64)
                    .then(|| DateTime::from_timestamp(ts as i64, 0))
                    .flatten();
            }
            return None;
        }
        _ => return None,
    };

    // Try ISO 8601 with timezone (e.g., "2024-01-15T10:30:00Z" or "2024-01-15T10:30:00+00:00")
    if let Ok(dt) = DateTime::parse_from_rfc3339(&s) {
        return Some(dt.with_timezone(&Utc));
    }

    // Try ISO 8601 without timezone: "2024-01-15T10:30:00"
    if let Ok(ndt) = NaiveDateTime::parse_from_str(&s, "%Y-%m-%dT%H:%M:%S") {
        return Some(ndt.and_utc());
    }

    // Try "YYYY-MM-DD HH:MM:SS"
    if let Ok(ndt) = NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S") {
        return Some(ndt.and_utc());
    }

    // Try "YYYY-MM-DD HH:MM"
    if let Ok(ndt) = NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M") {
        return Some(ndt.and_utc());
    }

    // Try date-only formats
    static DATE_FORMATS: &[&str] = &[
        "%Y-%m-%d",  // 2024-01-15
        "%d/%m/%Y",  // 15/01/2024
        "%m/%d/%Y",  // 01/15/2024
        "%Y/%m/%d",  // 2024/01/15
        "%B %d, %Y", // January 15, 2024
        "%b %d, %Y", // Jan 15, 2024
        "%d %B %Y",  // 15 January 2024
        "%d-%m-%Y",  // 15-01-2024
        "%m-%d-%Y",  // 01-15-2024
    ];

    for fmt in DATE_FORMATS {
        if let Ok(nd) = NaiveDate::parse_from_str(&s, fmt) {
            return nd.and_hms_opt(0, 0, 0).map(|ndt| ndt.and_utc());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_frontmatter() {
        let content = "\
---
title: My Note
tags: [rust, programming]
created: 2024-01-15
---
# Body content
";
        let fm = parse_frontmatter(content).expect("should parse");
        assert_eq!(fm.title.as_deref(), Some("My Note"));
        assert_eq!(fm.tags, vec!["rust", "programming"]);
        assert!(fm.created.is_some());
        assert_eq!(fm.created.unwrap().date_naive().to_string(), "2024-01-15");
    }

    #[test]
    fn test_yaml_list_tags() {
        let content = "\
---
tags:
  - productivity
  - note-taking
  - PKM
---
Body
";
        let fm = parse_frontmatter(content).expect("should parse");
        assert_eq!(fm.tags, vec!["productivity", "note-taking", "pkm"]);
    }

    #[test]
    fn test_single_tag_field() {
        let content = "\
---
tag: single-tag
---
Body
";
        let fm = parse_frontmatter(content).expect("should parse");
        assert_eq!(fm.tags, vec!["single-tag"]);
    }

    #[test]
    fn test_tags_with_hash_prefix() {
        let content = "\
---
tags: [\"#rust\", \"#code\"]
---
Body
";
        let fm = parse_frontmatter(content).expect("should parse");
        assert_eq!(fm.tags, vec!["rust", "code"]);
    }

    #[test]
    fn test_comma_separated_tags() {
        let content = "\
---
tags: \"alpha, beta, gamma\"
---
Body
";
        let fm = parse_frontmatter(content).expect("should parse");
        assert_eq!(fm.tags, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn test_iso_datetime() {
        let content = "\
---
created: \"2024-06-15T14:30:00Z\"
---
Body
";
        let fm = parse_frontmatter(content).expect("should parse");
        let dt = fm.created.unwrap();
        assert_eq!(dt.to_rfc3339(), "2024-06-15T14:30:00+00:00");
    }

    #[test]
    fn test_datetime_without_timezone() {
        let content = "\
---
created: \"2024-06-15T14:30:00\"
---
Body
";
        let fm = parse_frontmatter(content).expect("should parse");
        assert!(fm.created.is_some());
    }

    #[test]
    fn test_date_with_time() {
        let content = "\
---
created: \"2024-06-15 14:30:00\"
---
Body
";
        let fm = parse_frontmatter(content).expect("should parse");
        assert!(fm.created.is_some());
    }

    #[test]
    fn test_date_only() {
        let content = "\
---
created: 2024-06-15
---
Body
";
        let fm = parse_frontmatter(content).expect("should parse");
        assert!(fm.created.is_some());
    }

    #[test]
    fn test_modified_and_updated_aliases() {
        let content = "\
---
updated: 2024-07-01
---
Body
";
        let fm = parse_frontmatter(content).expect("should parse");
        assert!(fm.modified.is_some());
    }

    #[test]
    fn test_aliases() {
        let content = "\
---
aliases:
  - My Alias
  - Another Name
---
Body
";
        let fm = parse_frontmatter(content).expect("should parse");
        assert_eq!(fm.aliases, vec!["My Alias", "Another Name"]);
    }

    #[test]
    fn test_extra_fields() {
        let content = "\
---
title: Test
custom_field: hello
number_field: 42
---
Body
";
        let fm = parse_frontmatter(content).expect("should parse");
        assert!(fm.extra.contains_key("custom_field"));
        assert!(fm.extra.contains_key("number_field"));
        assert_eq!(
            fm.extra.get("custom_field"),
            Some(&Value::String("hello".to_string()))
        );
    }

    #[test]
    fn test_no_frontmatter() {
        let content = "# Just a heading\nSome content\n";
        assert!(parse_frontmatter(content).is_none());
    }

    #[test]
    fn test_empty_frontmatter() {
        let content = "\
---
---
Body
";
        let fm = parse_frontmatter(content).expect("should parse empty frontmatter");
        assert!(fm.title.is_none());
        assert!(fm.tags.is_empty());
    }

    #[test]
    fn test_malformed_yaml() {
        let content = "\
---
title: [unclosed bracket
  invalid: yaml: content: here
---
Body
";
        // Should return None for unparseable YAML
        assert!(parse_frontmatter(content).is_none());
    }

    #[test]
    fn test_frontmatter_without_closing() {
        let content = "\
---
title: Unclosed
Body content here
";
        assert!(parse_frontmatter(content).is_none());
    }

    #[test]
    fn test_strip_frontmatter_with_fm() {
        let content = "\
---
title: Test
---
# Body
Content here
";
        let body = strip_frontmatter(content);
        assert_eq!(body, "# Body\nContent here\n");
    }

    #[test]
    fn test_strip_frontmatter_without_fm() {
        let content = "# No frontmatter\nJust body\n";
        let body = strip_frontmatter(content);
        assert_eq!(body, content);
    }

    #[test]
    fn test_tags_normalized_lowercase() {
        let content = "\
---
tags: [Rust, PROGRAMMING, Mixed_Case]
---
Body
";
        let fm = parse_frontmatter(content).expect("should parse");
        assert_eq!(fm.tags, vec!["rust", "programming", "mixed_case"]);
    }

    #[test]
    fn test_both_tag_and_tags_merged() {
        let content = "\
---
tags: [alpha]
tag: beta
---
Body
";
        let fm = parse_frontmatter(content).expect("should parse");
        assert!(fm.tags.contains(&"alpha".to_string()));
        assert!(fm.tags.contains(&"beta".to_string()));
        assert_eq!(fm.tags.len(), 2);
    }

    #[test]
    fn test_date_from_date_field() {
        let content = "\
---
date: 2024-03-20
---
Body
";
        let fm = parse_frontmatter(content).expect("should parse");
        assert!(fm.created.is_some());
    }

    #[test]
    fn test_wikilink_wrapped_date() {
        let content = "\
---
created: '[[2025-08-04]]'
---
Body
";
        let fm = parse_frontmatter(content).expect("should parse");
        assert!(fm.created.is_some());
        assert_eq!(fm.created.unwrap().date_naive().to_string(), "2025-08-04");
    }

    #[test]
    fn test_frontmatter_wikilinks_from_people_field() {
        let content = "\
---
people: \"[[Alice]], [[Bob]]\"
---
Body
";
        let fm = parse_frontmatter(content).expect("should parse");
        assert!(fm.links.contains(&"alice".to_string()));
        assert!(fm.links.contains(&"bob".to_string()));
    }

    #[test]
    fn test_frontmatter_wikilinks_from_multiple_fields() {
        let content = "\
---
author: \"[[Charlie]]\"
related: \"[[Project Alpha]]\"
organization: \"[[Acme Corp]]\"
---
Body
";
        let fm = parse_frontmatter(content).expect("should parse");
        assert!(fm.links.contains(&"charlie".to_string()));
        assert!(fm.links.contains(&"project alpha".to_string()));
        assert!(fm.links.contains(&"acme corp".to_string()));
    }

    #[test]
    fn test_frontmatter_wikilinks_with_aliases() {
        let content = "\
---
people: \"[[Alice Smith|Alice]]\"
---
Body
";
        let fm = parse_frontmatter(content).expect("should parse");
        assert!(fm.links.contains(&"alice smith".to_string()));
        assert!(!fm.links.contains(&"alice".to_string()));
    }

    #[test]
    fn test_frontmatter_wikilinks_with_headings() {
        let content = "\
---
related: \"[[Note#Section]]\"
---
Body
";
        let fm = parse_frontmatter(content).expect("should parse");
        assert!(fm.links.contains(&"note".to_string()));
    }

    #[test]
    fn test_frontmatter_wikilinks_yaml_list() {
        let content = "\
---
people:
  - \"[[Alice]]\"
  - \"[[Bob]]\"
---
Body
";
        let fm = parse_frontmatter(content).expect("should parse");
        assert!(fm.links.contains(&"alice".to_string()));
        assert!(fm.links.contains(&"bob".to_string()));
    }

    #[test]
    fn test_frontmatter_wikilinks_deduplicated() {
        let content = "\
---
people: \"[[Alice]]\"
author: \"[[Alice]]\"
---
Body
";
        let fm = parse_frontmatter(content).expect("should parse");
        let alice_count = fm.links.iter().filter(|l| *l == "alice").count();
        assert_eq!(alice_count, 1);
    }

    #[test]
    fn test_frontmatter_no_wikilinks() {
        let content = "\
---
title: Plain Note
tags: [rust]
---
Body
";
        let fm = parse_frontmatter(content).expect("should parse");
        assert!(fm.links.is_empty());
    }

    #[test]
    fn test_configured_plain_frontmatter_values_promote_to_links() {
        let content = "\
---
people:
  - Jay Vidyarthi
  - Stefan Leßle
  - Jay Vidyarthi
projects: Product OS
count: 3
details:
  role: facilitator
---
Body
";
        let mut fm = parse_frontmatter(content).expect("should parse");
        fm.promote_link_fields(&["people".to_string()].into_iter().collect());

        assert_eq!(
            fm.links,
            vec!["jay vidyarthi".to_string(), "stefan leßle".to_string()]
        );
        assert!(!fm.links.contains(&"product os".to_string()));
        assert!(!fm.links.contains(&"facilitator".to_string()));
        assert!(!fm.links.contains(&"3".to_string()));
    }

    #[test]
    fn test_configured_field_keeps_literal_wikilinks_exact() {
        let content = "\
---
people:
  - \"[[Alice Smith|Alice]]\"
  - Bob Jones
---
Body
";
        let mut fm = parse_frontmatter(content).expect("should parse");
        fm.promote_link_fields(&["PEOPLE".to_string()].into_iter().collect());

        assert_eq!(
            fm.links,
            vec!["alice smith".to_string(), "bob jones".to_string()]
        );
    }

    #[test]
    fn test_frontmatter_wikilinks_date_field_not_extracted() {
        // Wikilinks in date fields like created: '[[2025-08-04]]' should still
        // be extracted as links — the date parser handles them separately
        let content = "\
---
created: '[[2025-08-04]]'
people: \"[[Alice]]\"
---
Body
";
        let fm = parse_frontmatter(content).expect("should parse");
        assert!(fm.links.contains(&"alice".to_string()));
        // The date wikilink is also extracted as a link — that's fine,
        // it will be filtered out by the date link filter in extract_entities
        assert!(fm.links.contains(&"2025-08-04".to_string()));
    }
}
