use std::collections::HashMap;

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
    /// All other key-value pairs not captured by named fields.
    pub extra: HashMap<String, Value>,
}

/// Parse YAML frontmatter delimited by `---` at the start of a document.
///
/// Returns `None` if the document has no valid frontmatter block.
pub fn parse_frontmatter(content: &str) -> Option<Frontmatter> {
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
        return Some(Frontmatter::default());
    }

    let closing = after_first_newline.find("\n---")?;
    let yaml_str = &after_first_newline[..closing];

    if yaml_str.trim().is_empty() {
        return Some(Frontmatter::default());
    }

    let mapping: HashMap<String, Value> = serde_yaml::from_str(yaml_str).ok()?;

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
    static CREATED_FIELDS: &[&str] = &["created", "date", "date_created", "date created", "createdAt", "created_at"];
    static MODIFIED_FIELDS: &[&str] = &["modified", "updated", "date_modified", "date modified", "lastModified", "last_modified", "updatedAt"];

    for &key in CREATED_FIELDS {
        if fm.created.is_some() { break; }
        if let Some(val) = mapping.get(key) {
            fm.created = value_to_datetime(val);
        }
    }
    for &key in MODIFIED_FIELDS {
        if fm.modified.is_some() { break; }
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

    Some(fm)
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

fn value_to_datetime(val: &Value) -> Option<DateTime<Utc>> {
    let s = match val {
        Value::String(s) => s.clone(),
        Value::Number(n) => {
            // Could be a unix timestamp
            if let Some(ts) = n.as_i64() {
                return DateTime::from_timestamp(ts, 0);
            }
            if let Some(ts) = n.as_f64() {
                return DateTime::from_timestamp(ts as i64, 0);
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
        "%Y-%m-%d",       // 2024-01-15
        "%d/%m/%Y",       // 15/01/2024
        "%m/%d/%Y",       // 01/15/2024
        "%Y/%m/%d",       // 2024/01/15
        "%B %d, %Y",      // January 15, 2024
        "%b %d, %Y",      // Jan 15, 2024
        "%d %B %Y",       // 15 January 2024
        "%d-%m-%Y",       // 15-01-2024
        "%m-%d-%Y",       // 01-15-2024
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
}
