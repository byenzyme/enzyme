//! The readable `.enzyme` language. Pure parsing/compilation shared by runtime and playground.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Profile {
    pub seek: String,
    pub notice: Vec<String>,
    #[serde(default)]
    pub ask: Vec<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub recognize: Vec<String>,
}
/// A limit on linked child entities, never on source evidence or query results.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SelectionAmount {
    Count { count: usize },
    Percent { percent: f64, up_to: Option<usize> },
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SelectionMode {
    Frequency,
    Recency,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Selection {
    pub amount: SelectionAmount,
    pub by: SelectionMode,
}
impl Selection {
    pub fn limit(&self, eligible: usize) -> usize {
        match self.amount {
            SelectionAmount::Count { count } => count.min(eligible),
            SelectionAmount::Percent { percent, up_to } => {
                // Use the canonical decimal ratio so e.g. 0.07% of 10,000 is
                // exactly seven, rather than rounding an f64 product up to eight.
                let decimal = percent.to_string();
                let (whole, fraction) = decimal.split_once('.').unwrap_or((&decimal, ""));
                let numerator: u128 = format!("{whole}{fraction}")
                    .parse()
                    .expect("valid percentage");
                let product = eligible as u128 * numerator;
                let count = 10u128
                    .checked_pow(fraction.len() as u32 + 2)
                    .map_or(usize::from(product > 0), |denominator| {
                        product.div_ceil(denominator) as usize
                    });
                count.min(eligible).min(up_to.unwrap_or(usize::MAX))
            }
        }
    }
}
/// Reference time for every time-dependent reading behavior in a vault.
/// A date is the end of that UTC day; absence keeps the wall clock.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AsOf {
    Date { date: String },
    LatestEvidence,
}
/// Calendar granularity for temporal evidence periods (UTC). `Auto` keeps the
/// default rule: months when dated history spans at most six months, else
/// quarters. Inert without a temporal sampling mode.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Periods {
    Auto,
    Monthly,
    Quarterly,
}
impl Periods {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Monthly => "monthly",
            Self::Quarterly => "quarterly",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Learning {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_of: Option<AsOf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<Selection>,
    pub budget: Option<usize>,
    pub sample: Option<String>,
    pub favor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub periods: Option<Periods>,
}
impl Learning {
    /// Canonical surface mode; storage retains the legacy fields for compatibility.
    pub fn sampling_mode(&self) -> Option<&'static str> {
        (self.sample.is_some() || self.favor.is_some()).then_some(
            if self.favor.as_deref() == Some("recent") {
                "recency"
            } else {
                "time"
            },
        )
    }
    pub fn over(&self, base: &Self) -> Self {
        let mut merged = Self {
            as_of: self.as_of.clone().or_else(|| base.as_of.clone()),
            selection: self.selection.clone().or_else(|| base.selection.clone()),
            budget: self.budget.or(base.budget),
            sample: self.sample.clone().or_else(|| base.sample.clone()),
            favor: self.favor.clone().or_else(|| base.favor.clone()),
            periods: self.periods.or(base.periods),
        };
        // Resolve legacy partial policies only after inheritance. New syntax sets
        // both fields atomically, so time overrides inherited recency completely.
        if merged.sampling_mode().is_some() {
            merged.sample = Some("across_time".into());
            merged.favor.get_or_insert_with(|| "equal".into());
        }
        merged
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Reading {
    pub entity: String,
    pub profile: String,
    pub definition: Option<Profile>,
    pub learning: Learning,
    pub include_linked_pages: bool,
    /// A SQLite source reading expands the links emitted by its `who` role.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_who_links: bool,
    /// `tags|links matching "…"`: `entity` holds a `*` glob that expands at job
    /// build time into one reading per matching indexed entity.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pattern: bool,
}
/// Case-insensitive name glob where `*` matches any run of characters, including
/// none. Every other character is literal. Tag and link names are indexed in
/// lower case, so this agrees with how explicit readings find their entity.
pub fn glob_matches(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.to_lowercase().chars().collect();
    let name: Vec<char> = name.to_lowercase().chars().collect();
    let (mut p, mut n) = (0, 0);
    let mut backtrack: Option<(usize, usize)> = None;
    while n < name.len() {
        if p < pattern.len() && pattern[p] == '*' {
            backtrack = Some((p, n));
            p += 1;
        } else if p < pattern.len() && pattern[p] == name[n] {
            p += 1;
            n += 1;
        } else if let Some((star, matched)) = backtrack {
            p = star + 1;
            n = matched + 1;
            backtrack = Some((star, matched + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Memory {
    pub destination: String,
    pub conditions: Vec<String>,
    pub guidance: Vec<String>,
}
/// `remember in folder "<dir>" [in source "<name>"] [when { … }] create note [{ … }]`.
///
/// Agent guidance, not a hook: it states where new notes belong. `folder` is
/// relative to the Markdown source root (`"."` is the root itself) and is never
/// absolute or `..`-escaping. `source` names the declaring Markdown source; it
/// is required only when a workspace declares several Markdown sources.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NotePolicy {
    pub folder: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub guidance: Vec<String>,
    /// Absolute Markdown root the folder is relative to. Filled by [`resolve`];
    /// always `None` in a parsed program.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
}
impl NotePolicy {
    /// The absolute write folder once resolved, e.g. `/notes/inbox`.
    pub fn resolved_folder(&self) -> Option<PathBuf> {
        self.root.as_ref().map(|root| {
            if self.folder == "." {
                PathBuf::from(root)
            } else {
                Path::new(root).join(&self.folder)
            }
        })
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Vault {
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
    pub path: String,
    pub learning: Learning,
    pub readings: Vec<Reading>,
    pub exclusions: Vec<String>,
    pub excluded_tags: Vec<String>,
    pub excluded_links: Vec<String>,
    pub frontmatter_link_fields: Vec<String>,
    pub embedding_limit: Option<usize>,
    pub format: Option<String>,
    pub targets: Vec<String>,
    pub memories: Vec<Memory>,
    pub retrieval: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub note_policies: Vec<NotePolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArenaChannel {
    pub remote_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Read compatibility for the first typed-source prototype.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    /// Read compatibility for the first typed-source prototype.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArenaSource {
    pub name: String,
    pub channels: Vec<ArenaChannel>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MarkdownSource {
    pub name: String,
    pub path: String,
}

/// A read-only query over an existing SQLite database. Column names refer to
/// unique aliases in the query result, not to columns in the source tables.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SqliteSource {
    pub name: String,
    pub db: String,
    pub query: String,
    pub id: Vec<String>,
    pub document_ref: Option<String>,
    pub who: SqliteWho,
    pub when: String,
    pub what: Vec<String>,
    pub where_columns: Vec<String>,
    pub weight: Option<String>,
    pub timestamp_unit: String,
    pub timestamp_epoch: Option<String>,
    /// Optional bundled content classifier applied after decoding query rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "format", rename_all = "snake_case")]
pub enum SqliteWho {
    Columns { columns: Vec<String> },
    JsonArray { column: String },
    Delimited { column: String, delimiter: String },
}

/// One value of a host source field: quoted text, a non-negative integer,
/// `true`/`false`, or a `{ "…" … }` list of strings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum HostValue {
    Bool(bool),
    Integer(u64),
    Text(String),
    List(Vec<String>),
}

/// `<words…> <value>` inside a host source. `key` holds the words joined by
/// single spaces, e.g. `backfill days`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostField {
    pub key: String,
    pub value: HostValue,
}

/// A source whose kind the language does not interpret, e.g.
/// `source google-mail "mail" { account "me@example.com" backfill days 365 }`.
/// The embedding host lowers it to a native source with
/// [`Program::lower_host_sources`] before [`resolve`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostSource {
    /// Serialized as `host_kind`: `kind` is the [`Source`] tag.
    #[serde(rename = "host_kind")]
    pub kind: String,
    pub name: String,
    /// Fields in declaration order; keys are unique.
    pub fields: Vec<HostField>,
}

impl HostSource {
    pub fn field(&self, key: &str) -> Option<&HostValue> {
        self.fields
            .iter()
            .find(|field| field.key == key)
            .map(|field| &field.value)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    Arena(ArenaSource),
    Sqlite(SqliteSource),
    Markdown(MarkdownSource),
    Host(HostSource),
}

impl Source {
    pub fn name(&self) -> &str {
        match self {
            Source::Arena(source) => &source.name,
            Source::Sqlite(source) => &source.name,
            Source::Markdown(source) => &source.name,
            Source::Host(source) => &source.name,
        }
    }

    /// The kind word as written: `markdown`, `sqlite`, `arena`, or the host kind.
    pub fn kind(&self) -> &str {
        match self {
            Source::Arena(_) => "arena",
            Source::Sqlite(_) => "sqlite",
            Source::Markdown(_) => "markdown",
            Source::Host(source) => &source.kind,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Workspace {
    pub name: String,
    pub sources: Vec<Source>,
    pub learning: Learning,
    pub readings: Vec<Reading>,
    pub format: Option<String>,
    #[serde(default)]
    pub targets: Vec<String>,
    #[serde(default)]
    pub exclusions: Vec<String>,
    #[serde(default)]
    pub excluded_tags: Vec<String>,
    #[serde(default)]
    pub excluded_links: Vec<String>,
    #[serde(default)]
    pub frontmatter_link_fields: Vec<String>,
    pub embedding_limit: Option<usize>,
    #[serde(default)]
    pub memories: Vec<Memory>,
    pub retrieval: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub note_policies: Vec<NotePolicy>,
}

impl Workspace {
    /// The workspace body as a vault. A workspace whose only source is one
    /// Markdown source keeps that path (legacy, path-keyed index identity);
    /// every other workspace is the named scope `workspace:<name>`.
    pub fn unresolved_vault(&self) -> Vault {
        let path = self
            .markdown_path()
            .map(str::to_string)
            .unwrap_or_else(|| format!("workspace:{}", self.name));
        Vault {
            path,
            learning: self.learning.clone(),
            readings: self.readings.clone(),
            format: self.format.clone(),
            targets: self.targets.clone(),
            exclusions: self.exclusions.clone(),
            excluded_tags: self.excluded_tags.clone(),
            excluded_links: self.excluded_links.clone(),
            frontmatter_link_fields: self.frontmatter_link_fields.clone(),
            embedding_limit: self.embedding_limit,
            memories: self.memories.clone(),
            retrieval: self.retrieval.clone(),
            note_policies: self.note_policies.clone(),
            ..Default::default()
        }
    }

    pub fn markdown_path(&self) -> Option<&str> {
        match self.sources.as_slice() {
            [Source::Markdown(source)] => Some(&source.path),
            _ => None,
        }
    }

    pub fn markdown_sources(&self) -> impl Iterator<Item = &MarkdownSource> {
        self.sources.iter().filter_map(|source| match source {
            Source::Markdown(source) => Some(source),
            _ => None,
        })
    }

    /// Sources a host must lower before [`resolve`].
    pub fn host_sources(&self) -> impl Iterator<Item = &HostSource> {
        self.sources.iter().filter_map(|source| match source {
            Source::Host(source) => Some(source),
            _ => None,
        })
    }

    /// The Markdown source a create-note policy writes into: its `in source`
    /// name, or the workspace's only Markdown source.
    pub fn note_policy_source(&self, policy: &NotePolicy) -> Option<&MarkdownSource> {
        let mut markdown = self.markdown_sources();
        match &policy.source {
            Some(name) => markdown.find(|source| source.name.eq_ignore_ascii_case(name)),
            None => {
                let only = markdown.next();
                markdown.next().is_none().then_some(only).flatten()
            }
        }
    }

    /// Whether a Markdown source is named by a create-note policy. Lowering
    /// marks exactly these sources writable in named workspaces.
    pub fn is_writable_markdown(&self, source_name: &str) -> bool {
        self.note_policies.iter().any(|policy| {
            self.note_policy_source(policy)
                .is_some_and(|source| source.name.eq_ignore_ascii_case(source_name))
        })
    }

    pub fn set_markdown_vault(&mut self, vault: Vault) {
        debug_assert!(self.markdown_path().is_some());
        self.set_body(vault);
    }

    /// Replace every body statement (readings, settings, exclusions, agent
    /// policies) from a vault, leaving the sources untouched.
    pub fn set_body(&mut self, vault: Vault) {
        self.note_policies = vault.note_policies;
        self.learning = vault.learning;
        self.readings = vault.readings;
        self.format = vault.format;
        self.targets = vault.targets;
        self.exclusions = vault.exclusions;
        self.excluded_tags = vault.excluded_tags;
        self.excluded_links = vault.excluded_links;
        self.frontmatter_link_fields = vault.frontmatter_link_fields;
        self.embedding_limit = vault.embedding_limit;
        self.memories = vault.memories;
        self.retrieval = vault.retrieval;
    }
}
impl Vault {
    /// The explicit reading for one entity; name patterns never match here.
    pub fn reading(&self, name: &str, kind: &str) -> Option<&Reading> {
        self.readings.iter().filter(|r| !r.pattern).find(|r| {
            let (k, n) = split_entity(&r.entity);
            k == (if kind == "append_log" { "log" } else { kind }) && n.eq_ignore_ascii_case(name)
        })
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    pub generation: Option<String>,
    pub local_model: Option<String>,
    pub updates: Option<bool>,
    pub embedding_limit: Option<usize>,
    pub minimum_tags: Option<usize>,
    pub minimum_links: Option<usize>,
    pub minimum_folders: Option<usize>,
    pub total_limit: Option<usize>,
    pub format: Option<String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Program {
    pub profiles: BTreeMap<String, Profile>,
    pub learning: Learning,
    pub settings: Settings,
    pub vaults: Vec<Vault>,
    #[serde(default)]
    pub workspaces: Vec<Workspace>,
    pub retrieval: Option<Vec<String>>,
}
impl Program {
    /// Let the embedding host replace its own source kinds with native sources
    /// before [`resolve`]. `lower` receives the workspace name and each host
    /// source in declaration order; `Ok(Some(source))` replaces it and
    /// `Ok(None)` leaves it in place (so `resolve` will reject it). A lowered
    /// source keeps the host source's name, so readings such as
    /// `learn questions from source "mail"` keep resolving.
    pub fn lower_host_sources(
        &mut self,
        mut lower: impl FnMut(&str, &HostSource) -> Result<Option<Source>>,
    ) -> Result<()> {
        for workspace in &mut self.workspaces {
            for source in &mut workspace.sources {
                let Source::Host(host) = source else {
                    continue;
                };
                let lowered = lower(&workspace.name, host).with_context(|| {
                    format!(
                        "lowering source {} {:?} in workspace {:?}",
                        host.kind, host.name, workspace.name
                    )
                })?;
                let Some(lowered) = lowered else { continue };
                ensure!(
                    !matches!(lowered, Source::Host(_)),
                    "source {} {:?} in workspace {:?} must lower to markdown, sqlite, or arena",
                    host.kind,
                    host.name,
                    workspace.name
                );
                ensure!(
                    lowered.name().eq_ignore_ascii_case(&host.name),
                    "source {} {:?} in workspace {:?} lowered to a source named {:?}; lowering must keep the name",
                    host.kind,
                    host.name,
                    workspace.name,
                    lowered.name()
                );
                *source = lowered;
            }
        }
        Ok(())
    }
}
#[derive(Debug, Clone)]
struct Token {
    value: String,
    string: bool,
    line: usize,
    col: usize,
}
fn lex(input: &str) -> Result<Vec<Token>> {
    let chars: Vec<char> = input.chars().collect();
    let (mut i, mut line, mut col) = (0, 1, 1);
    let mut out = Vec::new();
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            if c == '\n' {
                line += 1;
                col = 1;
            } else {
                col += 1;
            }
            i += 1;
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
                col += 1;
            }
            continue;
        }
        let start = (line, col);
        if c == '"' && chars.get(i + 1) == Some(&'"') && chars.get(i + 2) == Some(&'"') {
            i += 3;
            col += 3;
            let mut value = String::new();
            let mut closed = false;
            while i < chars.len() {
                if chars.get(i) == Some(&'"')
                    && chars.get(i + 1) == Some(&'"')
                    && chars.get(i + 2) == Some(&'"')
                {
                    i += 3;
                    col += 3;
                    closed = true;
                    break;
                }
                let x = chars[i];
                value.push(x);
                i += 1;
                if x == '\n' {
                    line += 1;
                    col = 1;
                } else {
                    col += 1;
                }
            }
            ensure!(closed, "{}:{}: unclosed multiline string", start.0, start.1);
            out.push(Token {
                value,
                string: true,
                line: start.0,
                col: start.1,
            });
        } else if c == '"' {
            let mut raw = String::from("\"");
            i += 1;
            col += 1;
            let mut closed = false;
            while i < chars.len() {
                let x = chars[i];
                raw.push(x);
                i += 1;
                col += 1;
                if x == '\n' {
                    bail!("{}:{}: strings must use escaped newlines", start.0, start.1)
                }
                if x == '\\' && i < chars.len() {
                    raw.push(chars[i]);
                    i += 1;
                    col += 1;
                } else if x == '"' {
                    closed = true;
                    break;
                }
            }
            ensure!(closed, "{}:{}: unclosed string", start.0, start.1);
            let value: String = serde_json::from_str(&raw)
                .with_context(|| format!("{}:{}: invalid string", start.0, start.1))?;
            out.push(Token {
                value,
                string: true,
                line: start.0,
                col: start.1,
            });
        } else if "{}[]=,%".contains(c) {
            out.push(Token {
                value: c.to_string(),
                string: false,
                line,
                col,
            });
            i += 1;
            col += 1;
        } else if c.is_ascii_alphanumeric() || c == '_' {
            let mut value = String::new();
            let punctuation = if c.is_ascii_digit() { "_-." } else { "_-" };
            while i < chars.len()
                && (chars[i].is_ascii_alphanumeric() || punctuation.contains(chars[i]))
            {
                value.push(chars[i]);
                i += 1;
                col += 1;
            }
            out.push(Token {
                value,
                string: false,
                line: start.0,
                col: start.1,
            });
        } else {
            bail!("{line}:{col}: unexpected character {c:?}")
        }
    }
    out.push(Token {
        value: "<end>".into(),
        string: false,
        line,
        col,
    });
    Ok(out)
}
struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    in_reading: bool,
}
/// Accept only real calendar dates written as `YYYY-MM-DD` in years 1–9999.
fn valid_date(date: &str) -> bool {
    let parts: Vec<&str> = date.split('-').collect();
    let [y, m, d] = parts[..] else { return false };
    if y.len() != 4 || m.len() != 2 || d.len() != 2 {
        return false;
    }
    let (Ok(y), Ok(m), Ok(d)) = (y.parse::<u32>(), m.parse::<u32>(), d.parse::<u32>()) else {
        return false;
    };
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let days = match m {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        1..=12 => 31,
        _ => return false,
    };
    y >= 1 && (1..=days).contains(&d)
}
impl Parser {
    fn t(&self) -> &Token {
        &self.tokens[self.pos]
    }
    fn at(&self, s: &str) -> bool {
        !self.t().string && self.t().value == s
    }
    fn eat(&mut self, s: &str) -> bool {
        if self.at(s) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn err<T>(&self, message: &str) -> Result<T> {
        bail!(
            "{}:{}: {message}; found {:?}",
            self.t().line,
            self.t().col,
            self.t().value
        )
    }
    fn need(&mut self, s: &str) -> Result<()> {
        if self.eat(s) {
            Ok(())
        } else {
            self.err(&format!("expected {s:?}"))
        }
    }
    fn string(&mut self) -> Result<String> {
        if !self.t().string || self.t().value.trim().is_empty() {
            return self.err("expected nonempty quoted text");
        };
        let s = self.t().value.clone();
        self.pos += 1;
        Ok(s)
    }
    fn word(&mut self) -> Result<String> {
        if self.t().string
            || !self
                .t()
                .value
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        {
            return self.err("expected a name");
        };
        let s = self.t().value.clone();
        self.pos += 1;
        Ok(s)
    }
    fn number(&mut self, min: usize, max: usize) -> Result<usize> {
        let n = self.t().value.parse::<usize>().ok();
        if self.t().string || !n.is_some_and(|v| v >= min && v <= max) {
            return self.err(&format!("expected integer {min}..{max}"));
        }
        self.pos += 1;
        Ok(n.unwrap())
    }
    fn list(&mut self) -> Result<Vec<String>> {
        let block = self.eat("{");
        let end = if block {
            "}"
        } else {
            self.need("[")?;
            "]"
        };
        let mut v = vec![];
        while !self.at(end) {
            v.push(self.string()?);
            if !block && !self.eat(",") {
                break;
            }
        }
        self.need(end)?;
        ensure!(!v.is_empty(), "lists must not be empty");
        Ok(v)
    }
    fn string_or_list(&mut self) -> Result<Vec<String>> {
        if self.at("[") {
            self.list()
        } else {
            Ok(vec![self.string()?])
        }
    }
    fn set<T>(&self, place: &mut Option<T>, value: T) -> Result<()> {
        if place.is_some() {
            return self.err("setting is repeated in this scope");
        }
        *place = Some(value);
        Ok(())
    }
    fn learn_setting(&mut self, l: &mut Learning) -> Result<()> {
        if self.at("as") {
            if self.in_reading {
                return self.err(
                    "as of applies to a whole vault; set it in the vault or global learning block",
                );
            }
            self.pos += 1;
            self.need("of")?;
            let as_of = if self.eat("latest") {
                self.need("evidence")?;
                AsOf::LatestEvidence
            } else {
                if !self.t().string {
                    return self.err(
                        "as of needs a quoted calendar date such as \"2024-10-01\", or latest evidence",
                    );
                }
                let date = self.string()?;
                if !valid_date(&date) {
                    return self.err(&format!(
                        "as of needs a calendar date \"YYYY-MM-DD\" or latest evidence, not {date:?}"
                    ));
                }
                AsOf::Date { date }
            };
            self.set(&mut l.as_of, as_of)
        } else if self.eat("select") {
            let raw = self.t().value.clone();
            ensure!(
                !self.t().string && raw.chars().next().is_some_and(|c| c.is_ascii_digit()),
                "selection must be numeric"
            );
            self.pos += 1;
            let amount = if self.eat("%") {
                ensure!(
                    raw.split('.').count() <= 2
                        && raw.split('.').all(
                            |part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit())
                        ),
                    "expected a percentage"
                );
                // Check the decimal boundary before f64 conversion: a value
                // just above 100 must not round down into the accepted range.
                let (whole, fraction) = raw.split_once('.').unwrap_or((&raw, ""));
                ensure!(
                    whole.parse::<u16>().ok().is_some_and(
                        |n| n < 100 || (n == 100 && fraction.chars().all(|c| c == '0'))
                    ),
                    "selection percentage must be at most 100"
                );
                let percent: f64 = raw.parse().context("expected a percentage")?;
                ensure!(
                    percent.is_finite() && percent > 0.0 && percent <= 100.0,
                    "selection percentage must be greater than 0 and at most 100"
                );
                SelectionAmount::Percent {
                    percent,
                    up_to: None,
                }
            } else {
                ensure!(
                    raw.chars().all(|c| c.is_ascii_digit()),
                    "selection count must be a positive integer; fractions require %"
                );
                let count: usize = raw.parse().context("expected a positive selection count")?;
                ensure!(count > 0, "selection count must be positive");
                SelectionAmount::Count { count }
            };
            self.need("by")?;
            let by = if self.eat("frequency") {
                SelectionMode::Frequency
            } else {
                self.need("recency")?;
                SelectionMode::Recency
            };
            let amount = if self.eat("up") {
                self.need("to")?;
                let cap = self.number(1, usize::MAX)?;
                match amount {
                    SelectionAmount::Percent { percent, .. } => SelectionAmount::Percent {
                        percent,
                        up_to: Some(cap),
                    },
                    _ => return self.err("up to is only allowed after a percentage selection"),
                }
            } else {
                amount
            };
            self.set(&mut l.selection, Selection { amount, by })
        } else if self.eat("sample") {
            if self.eat("by") {
                ensure!(
                    l.sample.is_none() && l.favor.is_none(),
                    "duplicate or mixed sampling policy; use one sample by time or sample by recency"
                );
                let favor = if self.eat("time") {
                    "equal"
                } else if self.eat("recency") {
                    "recent"
                } else {
                    return self.err("expected sample by time or sample by recency");
                };
                l.sample = Some("across_time".into());
                l.favor = Some(favor.into());
                Ok(())
            } else if self.eat("across") {
                self.need("time")?;
                self.set(&mut l.sample, "across_time".into())
            } else {
                self.err("expected sample by time, sample by recency, or legacy sample across time")
            }
        } else if self.eat("favor") {
            let value = if self.eat("recent") {
                self.need("periods")?;
                "recent"
            } else if self.eat("all") {
                self.need("periods")?;
                self.need("equally")?;
                "equal"
            } else {
                return self.err(
                    "expected legacy favor recent periods or favor all periods equally; prefer sample by recency or sample by time",
                );
            };
            self.set(&mut l.favor, value.into())
        } else if self.eat("periods") {
            let periods = if self.eat("auto") {
                Periods::Auto
            } else if self.eat("monthly") {
                Periods::Monthly
            } else if self.eat("quarterly") {
                Periods::Quarterly
            } else {
                return self.err("periods must be auto, monthly or quarterly");
            };
            self.set(&mut l.periods, periods)
        } else {
            if !self.at("question") {
                return self
                    .err("expected as of, select, sample, favor, periods, or question budget");
            }
            self.pos += 1;
            self.need("budget")?;
            let n = self.number(3, 10000)?;
            self.set(&mut l.budget, n)
        }
    }
    fn learning(&mut self) -> Result<Learning> {
        self.need("{")?;
        let mut l = Learning::default();
        while !self.at("}") {
            self.learn_setting(&mut l)?;
        }
        self.need("}")?;
        Ok(l)
    }
    fn profile(&mut self) -> Result<Profile> {
        self.need("{")?;
        let mut p = Profile::default();
        let mut seen = std::collections::BTreeSet::new();
        while !self.at("}") {
            let key = self.word()?;
            ensure!(seen.insert(key.clone()), "duplicate profile field {key}");
            match key.as_str() {
                "seek" => p.seek = self.string()?,
                "notice" => p.notice = self.list()?,
                "ask" => p.ask = self.list()?,
                "note" => p.note = Some(self.string()?),
                "recognize" => p.recognize = self.list()?,
                _ => return self.err("expected seek, notice, ask, note, or recognize"),
            }
        }
        self.need("}")?;
        ensure!(
            !p.seek.is_empty() && !p.notice.is_empty(),
            "profile needs seek and notice"
        );
        Ok(p)
    }
    fn reading(&mut self) -> Result<Vec<Reading>> {
        self.in_reading = true;
        let readings = self.reading_body();
        self.in_reading = false;
        readings
    }
    fn reading_body(&mut self) -> Result<Vec<Reading>> {
        self.need("learn")?;
        self.need("questions")?;
        self.need("from")?;
        let kind = self.word()?;
        ensure!(
            [
                "folder",
                "folders",
                "tag",
                "tags",
                "link",
                "links",
                "log",
                "logs",
                "channel",
                "channels",
                "collection",
                "collections",
                "source",
                "thread"
            ]
            .contains(&kind.as_str()),
            "unknown reading kind {kind}"
        );
        let pattern = self.at("matching");
        let mut names = if pattern {
            if kind != "tags" && kind != "links" {
                return self.err("matching applies only to tags and links");
            }
            self.pos += 1;
            let glob = self.string()?;
            if !glob.contains('*') {
                return self.err(&format!(
                    "pattern {glob:?} has no *; use learn questions from {} {glob:?} for one {}",
                    kind.trim_end_matches('s'),
                    kind.trim_end_matches('s')
                ));
            }
            vec![glob]
        } else if kind.ends_with('s') {
            self.list()?
        } else if kind == "channel"
            && !self.t().string
            && !self.t().value.is_empty()
            && self
                .t()
                .value
                .chars()
                .all(|character| character.is_ascii_digit())
        {
            let id = self.t().value.clone();
            self.pos += 1;
            vec![id]
        } else {
            vec![self.string()?]
        };
        if kind == "thread" {
            self.need("in")?;
            self.need("source")?;
            let source = self.string()?;
            names = names
                .into_iter()
                .map(|name| format!("{source}/{name}"))
                .collect();
        }
        let kind = kind.trim_end_matches('s');
        let mut include = false;
        let mut include_who_links = false;
        if self.eat("including") {
            if self.eat("linked") {
                self.need("pages")?;
                include = true;
            } else {
                self.need("who")?;
                self.need("links")?;
                include_who_links = true;
            }
        }
        let (profile, definition) = if self.eat("about") {
            if self.eat("profile") {
                ("inline".into(), Some(self.profile()?))
            } else {
                (self.word()?, None)
            }
        } else {
            ("auto".into(), None)
        };
        let mut learning = if self.at("{") {
            self.learning()?
        } else {
            Learning::default()
        };
        while [
            "sample",
            "favor",
            "question",
            "including",
            "select",
            "periods",
        ]
        .iter()
        .any(|s| self.at(s))
        {
            if self.eat("including") {
                if self.eat("linked") {
                    ensure!(!include, "duplicate including linked pages");
                    self.need("pages")?;
                    include = true;
                } else {
                    ensure!(!include_who_links, "duplicate including who links");
                    self.need("who")?;
                    self.need("links")?;
                    include_who_links = true;
                }
            } else {
                self.learn_setting(&mut learning)?;
            }
        }
        ensure!(
            !include || kind == "folder",
            "only folders can include linked pages"
        );
        ensure!(
            !include_who_links || kind == "source",
            "only SQLite sources can include who links"
        );
        ensure!(
            learning.selection.is_none() || kind == "folder" || pattern || include_who_links,
            "select applies only to linked children of folders, who links of sources, and tags/links matching a pattern"
        );
        Ok(names
            .into_iter()
            .map(|name| Reading {
                entity: entity_selector(kind, &name),
                profile: profile.clone(),
                definition: definition.clone(),
                learning: learning.clone(),
                include_linked_pages: include,
                include_who_links,
                pattern,
            })
            .collect())
    }
    fn retrieval(&mut self) -> Result<Vec<String>> {
        self.need("when")?;
        self.need("asked")?;
        self.need("{")?;
        let mut steps = vec![];
        while !self.at("}") {
            if self.t().string {
                steps.push(self.string()?);
            } else if self.eat("retrieve") {
                self.need("passages")?;
                if self.eat("across") {
                    self.need("the")?;
                    self.need("vault")?;
                }
                self.need("through")?;
                self.need("learned")?;
                self.need("questions")?;
                steps.push("Run `enzyme petri --query` with the request. Use relevant learned-question vocabulary together with the request in `enzyme catalyze` to retrieve source passages across the vault. Questions are search handles, not evidence or answers.".into());
            } else {
                self.need("answer")?;
                self.need("with")?;
                self.need("sources")?;
                steps.push("Answer from retrieved passages and cite their source files.".into());
            }
        }
        self.need("}")?;
        ensure!(!steps.is_empty(), "when asked needs instructions");
        Ok(steps)
    }
    fn memory(&mut self) -> Result<Memory> {
        self.need("remember")?;
        self.need("in")?;
        let destination = self.string()?;
        self.need("when")?;
        self.need("{")?;
        let (mut conditions, mut guidance) = (vec![], vec![]);
        let mut append = false;
        while !self.at("}") {
            if self.t().string {
                conditions.push(self.string()?)
            } else {
                self.need("append")?;
                ensure!(!append, "only one append block is allowed");
                append = true;
                self.need("observation")?;
                self.need("{")?;
                while !self.at("}") {
                    guidance.push(self.string()?)
                }
                self.need("}")?;
            }
        }
        self.need("}")?;
        ensure!(
            !conditions.is_empty() && !guidance.is_empty(),
            "remember needs conditions and append guidance"
        );
        Ok(Memory {
            destination,
            conditions,
            guidance,
        })
    }
    fn settings(&mut self) -> Result<Settings> {
        self.need("{")?;
        let mut s = Settings::default();
        while !self.at("}") {
            if self.eat("generation") {
                let value = self.word()?;
                ensure!(
                    ["auto", "local", "hosted"].contains(&value.as_str()),
                    "unknown generation mode"
                );
                self.set(&mut s.generation, value)?;
            } else if self.eat("model") {
                let v = self.string()?;
                self.set(&mut s.local_model, v)?;
            } else if self.eat("updates") {
                let v = if self.eat("enabled") {
                    true
                } else {
                    self.need("disabled")?;
                    false
                };
                self.set(&mut s.updates, v)?;
            } else if self.eat("embedding") {
                self.need("limit")?;
                let v = self.number(0, 10_000_000)?;
                self.set(&mut s.embedding_limit, v)?;
            } else if self.eat("format") {
                let v = self.word()?;
                ensure!(
                    ["question", "thesis", "claim"].contains(&v.as_str()),
                    "unknown catalyst format"
                );
                self.set(&mut s.format, v)?;
            } else {
                self.need("selection")?;
                let k = self.word()?;
                let v = self.number(0, 10000)?;
                match k.as_str() {
                    "tags" => self.set(&mut s.minimum_tags, v)?,
                    "links" => self.set(&mut s.minimum_links, v)?,
                    "folders" => self.set(&mut s.minimum_folders, v)?,
                    "limit" => self.set(&mut s.total_limit, v)?,
                    _ => return self.err("unknown selection setting"),
                }
            }
        }
        self.need("}")?;
        Ok(s)
    }
    fn vault(&mut self) -> Result<Vault> {
        self.need("vault")?;
        let path = self.string()?;
        self.need("{")?;
        self.vault_body(path)
    }

    fn vault_body(&mut self, path: String) -> Result<Vault> {
        let mut v = Vault {
            path,
            ..Default::default()
        };
        let mut seen = std::collections::BTreeSet::new();
        while !self.at("}") {
            self.body_statement(&mut v, &mut seen, false)?;
        }
        self.need("}")?;
        Ok(v)
    }

    /// One statement shared by `vault` and `workspace` bodies. Source-backed
    /// readings and `in source` qualifiers exist only inside workspaces.
    fn body_statement(
        &mut self,
        v: &mut Vault,
        seen: &mut std::collections::BTreeSet<&'static str>,
        in_workspace: bool,
    ) -> Result<()> {
        {
            if self.at("learn") {
                let readings = self.reading()?;
                ensure!(
                    in_workspace
                        || readings.iter().all(|reading| !matches!(
                            split_entity(&reading.entity).0,
                            "channel" | "source" | "thread"
                        )),
                    "channel, source, and thread readings require a workspace source declaration"
                );
                v.readings.extend(readings);
            } else if self.eat("learning") {
                ensure!(seen.insert("learning"), "duplicate vault learning block");
                let l = self.learning()?;
                ensure!(
                    v.learning == Learning::default(),
                    "use a single learning block per scope"
                );
                v.learning = l;
            } else if ["sample", "favor", "question", "select", "as", "periods"]
                .iter()
                .any(|s| self.at(s))
            {
                self.learn_setting(&mut v.learning)?;
            } else if self.eat("leave") {
                self.need("out")?;
                let kind = self.word()?;
                ensure!(
                    seen.insert(match kind.as_str() {
                        "folders" => "excluded_folders",
                        "tags" => "excluded_tags",
                        "links" => "excluded_links",
                        _ => return self.err("leave out folders, tags, or links"),
                    }),
                    "duplicate exclusion list"
                );
                let values = self.list()?;
                match kind.as_str() {
                    "folders" => v.exclusions = values,
                    "tags" => v.excluded_tags = values,
                    _ => v.excluded_links = values,
                }
            } else if self.eat("prepare") {
                self.need("up")?;
                self.need("to")?;
                let n = self.number(0, 10_000_000)?;
                self.need("documents")?;
                self.need("per")?;
                self.need("run")?;
                self.need("newest")?;
                self.need("first")?;
                self.set(&mut v.embedding_limit, n)?;
            } else if self.eat("project") {
                self.need("questions")?;
                self.need("into")?;
                v.targets.push(self.string()?);
            } else if self.eat("references") {
                self.need("in")?;
                self.need("fields")?;
                ensure!(seen.insert("fields"), "duplicate reference fields");
                v.frontmatter_link_fields = self.list()?;
            } else if self.eat("produce") {
                let x = self.word()?;
                let f = match x.as_str() {
                    "questions" => "question",
                    "theses" => "thesis",
                    "claims" => "claim",
                    _ => return self.err("produce questions, theses, or claims"),
                };
                self.set(&mut v.format, f.into())?;
            } else if self.at("remember") {
                if self
                    .tokens
                    .get(self.pos + 2)
                    .is_some_and(|token| !token.string && token.value == "folder")
                {
                    let policy = self.note_policy()?;
                    ensure!(
                        in_workspace || policy.source.is_none(),
                        "in source applies only inside a workspace"
                    );
                    ensure!(
                        !v.note_policies.iter().any(|existing| existing.folder
                            == policy.folder
                            && existing.source.as_deref().map(str::to_lowercase)
                                == policy.source.as_deref().map(str::to_lowercase)),
                        "duplicate create note policy for folder {:?}",
                        policy.folder
                    );
                    v.note_policies.push(policy);
                } else {
                    v.memories.push(self.memory()?);
                }
            } else if self.at("when") {
                let r = self.retrieval()?;
                self.set(&mut v.retrieval, r)?;
            } else {
                return self.err(if in_workspace {
                    "expected a source, a reading, learning settings, exclusions, target, or agent policy"
                } else {
                    "expected a reading, learning settings, exclusions, target, or agent policy"
                });
            }
        }
        Ok(())
    }

    fn note_policy(&mut self) -> Result<NotePolicy> {
        self.need("remember")?;
        self.need("in")?;
        self.need("folder")?;
        let folder = self.string()?;
        if let Err(problem) = check_note_folder(&folder) {
            return self.err(&problem);
        }
        let source = if self.eat("in") {
            self.need("source")?;
            Some(self.string()?)
        } else {
            None
        };
        let mut conditions = vec![];
        if self.eat("when") {
            self.need("{")?;
            while !self.at("}") {
                conditions.push(self.string()?);
            }
            self.need("}")?;
            ensure!(
                !conditions.is_empty(),
                "remember in folder … when needs at least one condition"
            );
        }
        if !self.at("create") {
            return self.err("expected create note");
        }
        self.pos += 1;
        self.need("note")?;
        let mut guidance = vec![];
        if self.eat("{") {
            while !self.at("}") {
                guidance.push(self.string()?);
            }
            self.need("}")?;
            ensure!(!guidance.is_empty(), "create note guidance must not be empty");
        }
        Ok(NotePolicy {
            folder,
            source,
            conditions,
            guidance,
            root: None,
        })
    }

    fn markdown_source(&mut self) -> Result<Source> {
        self.need("source")?;
        self.need("markdown")?;
        let name = self.string()?;
        self.need("{")?;
        self.need("path")?;
        let path = self.string()?;
        self.need("}")?;
        ensure!(
            !path.trim().is_empty(),
            "Markdown source path must not be empty"
        );
        Ok(Source::Markdown(MarkdownSource { name, path }))
    }

    fn arena_source(&mut self) -> Result<Source> {
        self.need("source")?;
        self.need("arena")?;
        let name = self.string()?;
        self.need("{")?;
        self.need("channels")?;
        self.need("{")?;
        let mut channels = Vec::new();
        let mut ids = std::collections::BTreeSet::new();
        let mut aliases = std::collections::BTreeSet::new();
        let mut slugs = std::collections::BTreeSet::new();
        while !self.at("}") {
            self.need("channel")?;
            let remote_id = self.t().value.clone();
            ensure!(
                !self.t().string
                    && !remote_id.is_empty()
                    && remote_id.chars().all(|ch| ch.is_ascii_digit()),
                "Are.na channel ID must be numeric"
            );
            self.pos += 1;
            let (alias, slug, title) = if self.eat("as") {
                (Some(self.string()?), None, None)
            } else if self.eat("slug") {
                let slug = self.string()?;
                self.need("title")?;
                let title = self.string()?;
                (None, Some(slug), Some(title))
            } else {
                (None, None, None)
            };
            ensure!(
                ids.insert(remote_id.clone()),
                "duplicate Are.na channel ID {remote_id}"
            );
            if let Some(alias) = &alias {
                ensure!(
                    aliases.insert(alias.to_lowercase()),
                    "duplicate Are.na channel alias {alias}"
                );
            }
            if let Some(slug) = &slug {
                ensure!(
                    slugs.insert(slug.to_lowercase()),
                    "duplicate Are.na channel slug {slug}"
                );
            }
            channels.push(ArenaChannel {
                remote_id,
                alias,
                slug,
                title,
            });
        }
        self.need("}")?;
        self.need("}")?;
        ensure!(
            !channels.is_empty(),
            "Are.na source needs at least one channel"
        );
        Ok(Source::Arena(ArenaSource { name, channels }))
    }

    fn sqlite_source(&mut self) -> Result<Source> {
        self.need("source")?;
        self.need("sqlite")?;
        let name = self.string()?;
        self.need("{")?;
        let (mut db, mut query, mut id, mut who, mut when, mut what, mut where_columns) =
            (None, None, None, None, None, None, None);
        let (mut document_ref, mut weight, mut filter) = (None, None, None);
        let (mut timestamp_unit, mut timestamp_epoch) = (None, None);
        while !self.at("}") {
            if self.eat("database") {
                let value = self.string()?;
                self.set(&mut db, value)?;
            } else if self.eat("query") {
                let value = self.string()?;
                self.set(&mut query, value)?;
            } else if self.eat("id") {
                let value = self.string_or_list()?;
                self.set(&mut id, value)?;
            } else if self.eat("document") {
                self.need("ref")?;
                let value = self.string()?;
                self.set(&mut document_ref, value)?;
            } else if self.eat("who") {
                let value = if self.eat("json_array") {
                    SqliteWho::JsonArray {
                        column: self.string()?,
                    }
                } else if self.eat("delimited") {
                    let column = self.string()?;
                    self.need("by")?;
                    SqliteWho::Delimited {
                        column,
                        delimiter: self.string()?,
                    }
                } else {
                    SqliteWho::Columns {
                        columns: self.string_or_list()?,
                    }
                };
                self.set(&mut who, value)?;
            } else if self.eat("when") {
                let value = self.string()?;
                self.set(&mut when, value)?;
                self.need("unit")?;
                let unit = self.word()?;
                ensure!(
                    ["s", "ms", "us", "ns"].contains(&unit.as_str()),
                    "SQLite timestamp unit must be s, ms, us, or ns"
                );
                timestamp_unit = Some(unit);
                if self.eat("epoch") {
                    timestamp_epoch = Some(self.string()?);
                }
            } else if self.eat("what") {
                let value = self.string_or_list()?;
                self.set(&mut what, value)?;
            } else if self.eat("where") {
                let value = self.string_or_list()?;
                self.set(&mut where_columns, value)?;
            } else if self.eat("weight") {
                let value = self.string()?;
                self.set(&mut weight, value)?;
            } else if self.eat("filter") {
                let value = self.string()?;
                ensure!(
                    value == "apple-notes-content-v1",
                    "unknown SQLite source filter {value:?}"
                );
                self.set(&mut filter, value)?;
            } else {
                return self.err(
                    "expected database, query, id, document ref, who, when, what, where, weight, or filter in SQLite source",
                );
            }
        }
        self.need("}")?;
        let required = |value: Option<String>, field: &str| -> Result<String> {
            value
                .filter(|v| !v.trim().is_empty())
                .with_context(|| format!("SQLite source {name:?} needs {field}"))
        };
        Ok(Source::Sqlite(SqliteSource {
            name: name.clone(),
            db: required(db, "database")?,
            query: required(query, "query")?,
            id: id.context("SQLite source needs id")?,
            document_ref,
            who: who.unwrap_or(SqliteWho::Columns {
                columns: Vec::new(),
            }),
            when: required(when, "when")?,
            what: what.context("SQLite source needs what")?,
            where_columns: where_columns.unwrap_or_default(),
            weight,
            timestamp_unit: timestamp_unit.context("SQLite source needs when unit")?,
            timestamp_epoch,
            filter,
        }))
    }

    /// `source <kind> "name" { <words…> <value> … }` for any kind the language
    /// does not interpret itself. Values are text, integers, booleans, or lists.
    fn host_source(&mut self) -> Result<Source> {
        self.need("source")?;
        let kind = self.word()?;
        let name = self.string()?;
        self.need("{")?;
        let mut fields: Vec<HostField> = Vec::new();
        while !self.at("}") {
            let mut words = vec![self.word()?];
            while !self.t().string
                && !self.at("{")
                && !self.at("[")
                && !self.at("}")
                && !self.at("true")
                && !self.at("false")
                && !self.t().value.starts_with(|c: char| c.is_ascii_digit())
            {
                words.push(self.word()?);
            }
            let key = words.join(" ");
            let value = if self.t().string {
                HostValue::Text(self.string()?)
            } else if self.eat("true") {
                HostValue::Bool(true)
            } else if self.eat("false") {
                HostValue::Bool(false)
            } else if self.at("{") || self.at("[") {
                HostValue::List(self.list()?)
            } else if self.at("}") {
                return self.err(&format!("field {key:?} needs a value"));
            } else {
                let Ok(n) = self.t().value.parse::<u64>() else {
                    return self.err(&format!("field {key:?} needs a non-negative integer"));
                };
                self.pos += 1;
                HostValue::Integer(n)
            };
            if fields.iter().any(|field| field.key == key) {
                bail!("source {kind} {name:?} repeats field {key:?}");
            }
            fields.push(HostField { key, value });
        }
        self.need("}")?;
        Ok(Source::Host(HostSource { kind, name, fields }))
    }

    fn workspace(&mut self) -> Result<Workspace> {
        self.need("workspace")?;
        let name = self.string()?;
        self.need("{")?;
        let mut workspace = Workspace {
            name,
            ..Default::default()
        };
        let mut body = Vault::default();
        let mut seen = std::collections::BTreeSet::new();
        let mut source_names = std::collections::BTreeSet::new();
        while !self.at("}") {
            if self.at("source") {
                let source = match self.tokens.get(self.pos + 1) {
                    Some(token) if token.string => {
                        return self.err("expected a source kind such as markdown or sqlite");
                    }
                    Some(token) if token.value == "arena" => self.arena_source()?,
                    Some(token) if token.value == "sqlite" => self.sqlite_source()?,
                    Some(token) if token.value == "markdown" => self.markdown_source()?,
                    _ => self.host_source()?,
                };
                let source_name = source.name();
                ensure!(
                    source_names.insert(source_name.to_lowercase()),
                    "duplicate workspace source {source_name}"
                );
                workspace.sources.push(source);
            } else {
                self.body_statement(&mut body, &mut seen, true)?;
            }
        }
        self.need("}")?;
        ensure!(!workspace.sources.is_empty(), "workspace needs a source");
        workspace.set_body(body);
        let markdown: Vec<&str> = workspace
            .markdown_sources()
            .map(|source| source.name.as_str())
            .collect();
        for policy in &workspace.note_policies {
            match &policy.source {
                Some(source) => ensure!(
                    markdown.iter().any(|name| name.eq_ignore_ascii_case(source)),
                    "create note in source {source:?}: workspace {:?} declares no Markdown source with that name",
                    workspace.name
                ),
                None => ensure!(
                    markdown.len() == 1,
                    "create note in folder {:?} needs in source \"…\" naming {}",
                    policy.folder,
                    if markdown.is_empty() {
                        format!(
                            "a Markdown source; workspace {:?} declares none",
                            workspace.name
                        )
                    } else {
                        format!("one of the Markdown sources {}", markdown.join(", "))
                    }
                ),
            }
        }
        Ok(workspace)
    }
}

/// Create-note folders stay inside their Markdown source.
fn check_note_folder(folder: &str) -> std::result::Result<(), String> {
    let windows_drive = folder.len() >= 2 && folder.as_bytes()[1] == b':';
    if folder.starts_with(['/', '\\', '~']) || windows_drive {
        return Err(format!(
            "create note folder {folder:?} must be relative to the Markdown source root; use \".\" for the root"
        ));
    }
    if folder.split(['/', '\\']).any(|part| part == "..") {
        return Err(format!(
            "create note folder {folder:?} must not leave the Markdown source root"
        ));
    }
    if folder != folder.trim() {
        return Err(format!(
            "create note folder {folder:?} must not have surrounding whitespace"
        ));
    }
    Ok(())
}
pub fn parse(source: &str) -> Result<Program> {
    let mut p = Parser {
        tokens: lex(source)?,
        pos: 0,
        in_reading: false,
    };
    let mut out = Program::default();
    let mut settings = false;
    let mut learning = false;
    while !p.at("<end>") {
        if p.eat("profile") {
            let name = p.word()?;
            ensure!(name != "auto", "auto is reserved");
            let profile = p.profile()?;
            ensure!(
                out.profiles.insert(name.clone(), profile).is_none(),
                "duplicate profile {name}"
            );
        } else if p.eat("let") {
            let name = p.word()?;
            ensure!(name != "auto", "auto is reserved");
            p.need("=")?;
            p.need("profile")?;
            let profile = p.profile()?;
            ensure!(
                out.profiles.insert(name.clone(), profile).is_none(),
                "duplicate profile {name}"
            );
        } else if p.eat("learning") {
            ensure!(!learning, "duplicate global learning block");
            learning = true;
            out.learning = p.learning()?;
        } else if p.eat("settings") {
            ensure!(!settings, "duplicate settings block");
            settings = true;
            out.settings = p.settings()?;
        } else if p.at("vault") {
            out.vaults.push(p.vault()?);
        } else if p.at("workspace") {
            out.workspaces.push(p.workspace()?);
        } else if p.at("when") {
            let x = p.retrieval()?;
            p.set(&mut out.retrieval, x)?;
        } else {
            return p
                .err("expected profile, let, settings, learning, workspace, vault, or when asked");
        }
    }
    Ok(out)
}
pub fn builtin(name: &str) -> Option<&str> {
    Some(match name {
        "relationships" | "relational" => "relational",
        "decisions" | "decision_trace" => "decision_trace",
        "resonance" | "resonance_trace" => "resonance_trace",
        "preferences" | "preference_evidence" => "preference_evidence",
        "reflection" | "reflective" => "reflective",
        "tensions" | "tension_trace" => "tension_trace",
        "operational" => "operational",
        "auto" => "auto",
        _ => return None,
    })
}
pub fn entity_selector(kind: &str, name: &str) -> String {
    match kind {
        "tag" => format!("#{}", name.trim_start_matches('#')),
        "link" => format!(
            "[[{}]]",
            name.trim_start_matches("[[").trim_end_matches("]]")
        ),
        _ => format!("{kind}:{name}"),
    }
}

fn sqlite_source_key(name: &str) -> String {
    let mut escaped = String::new();
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
            escaped.push(char::from(byte));
        } else {
            escaped.push_str(&format!("%{byte:02X}"));
        }
    }
    escaped
}
/// Rebuilding readings from a concrete entity list must not drop name patterns;
/// they return to their declared positions so precedence and rendering hold.
pub fn retain_patterns(previous: &[Reading], rebuilt: &mut Vec<Reading>) {
    for (index, reading) in previous.iter().enumerate().filter(|(_, r)| r.pattern) {
        rebuilt.insert(index.min(rebuilt.len()), reading.clone());
    }
}
pub fn split_entity(entity: &str) -> (&str, &str) {
    if let Some(s) = entity.strip_prefix('#') {
        ("tag", s)
    } else if let Some(s) = entity.strip_prefix("[[").and_then(|s| s.strip_suffix("]]")) {
        ("link", s)
    } else {
        entity.split_once(':').unwrap_or(("link", entity))
    }
}
fn expand(path: &str, home: &Path) -> String {
    if path == "~" {
        home.to_string_lossy().into_owned()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home.join(rest).to_string_lossy().into_owned()
    } else {
        path.into()
    }
}

/// Whether `folder` (a root-relative path) is inside one of `exclusions`.
fn folder_is_excluded(folder: &str, exclusions: &[String]) -> bool {
    exclusions
        .iter()
        .any(|ex| folder == ex || folder.starts_with(&format!("{ex}/")))
}

fn lower_workspace(workspace: Workspace) -> Result<Vault> {
    if let Some(host) = workspace.host_sources().next() {
        bail!(
            "source {} {:?} in workspace {:?} is a host source; the host must lower it before resolution",
            host.kind,
            host.name,
            workspace.name
        );
    }
    let mut vault = workspace.unresolved_vault();
    let markdown: Vec<&MarkdownSource> = workspace.markdown_sources().collect();
    for policy in &mut vault.note_policies {
        let source = workspace
            .note_policy_source(policy)
            .with_context(|| {
                format!(
                    "create note in folder {:?} names no Markdown source of workspace {:?}",
                    policy.folder, workspace.name
                )
            })?;
        policy.root = Some(source.path.clone());
    }
    if workspace.markdown_path().is_some() {
        // A lone Markdown source keeps the legacy path-keyed lowering.
        return Ok(vault);
    }
    let mut channels = BTreeMap::<String, String>::new();
    let mut sqlite_sources = BTreeMap::<String, String>::new();
    for source in &workspace.sources {
        let name = source.name();
        ensure!(
            !name.contains('/') && !name.contains(':') && !name.contains('\\'),
            "source name must not contain /, \\, or :"
        );
        match source {
            Source::Arena(source) => {
                for channel in &source.channels {
                    let stable = format!("arena:{}/channel/{}", source.name, channel.remote_id);
                    let mut selectors = vec![channel.remote_id.as_str(), stable.as_str()];
                    if let Some(alias) = channel.alias.as_deref() {
                        selectors.push(alias);
                    }
                    if let Some(slug) = channel.slug.as_deref() {
                        selectors.push(slug);
                    }
                    if let Some(title) = channel.title.as_deref() {
                        selectors.push(title);
                    }
                    for selector in selectors {
                        if let Some(existing) =
                            channels.insert(selector.to_lowercase(), stable.clone())
                        {
                            ensure!(
                                existing == stable,
                                "ambiguous channel selector {selector:?} in workspace {}",
                                workspace.name
                            );
                        }
                    }
                }
            }
            Source::Sqlite(source) => {
                sqlite_sources.insert(source.name.to_lowercase(), source.name.clone());
            }
            Source::Markdown(_) | Source::Host(_) => {}
        }
    }
    // SQLite `where` labels index as unprefixed folder entities.
    let sqlite_folders = workspace
        .sources
        .iter()
        .any(|source| matches!(source, Source::Sqlite(s) if !s.where_columns.is_empty()));
    for reading in &mut vault.readings {
        let (kind, selector) = split_entity(&reading.entity);
        match kind {
            "channel" | "collection" => {
                let stable = channels.get(&selector.to_lowercase()).with_context(|| {
                    format!(
                        "channel {selector:?} is not declared by workspace {}",
                        workspace.name
                    )
                })?;
                reading.entity = entity_selector("collection", stable);
            }
            "source" => {
                let name = sqlite_sources
                    .get(&selector.to_lowercase())
                    .with_context(|| {
                        format!(
                            "SQLite source {selector:?} is not declared by workspace {}",
                            workspace.name
                        )
                    })?;
                if reading.include_who_links {
                    let source = workspace
                        .sources
                        .iter()
                        .find_map(|source| match source {
                            Source::Sqlite(source) if source.name == *name => Some(source),
                            _ => None,
                        })
                        .expect("resolved SQLite source exists");
                    ensure!(
                        !matches!(&source.who, SqliteWho::Columns { columns } if columns.is_empty()),
                        "SQLite source {name:?} needs a who mapping for including who links"
                    );
                }
                reading.entity =
                    entity_selector("collection", &format!("sqlite:{}", sqlite_source_key(name)));
            }
            "thread" => {
                let (source, thread) = selector
                    .split_once('/')
                    .context("thread reading needs a source name and thread")?;
                let name = sqlite_sources
                    .get(&source.to_lowercase())
                    .with_context(|| {
                        format!(
                            "SQLite source {source:?} is not declared by workspace {}",
                            workspace.name
                        )
                    })?;
                reading.entity = entity_selector(
                    "collection",
                    &format!("sqlite:{}/thread/{thread}", sqlite_source_key(name)),
                );
            }
            "folder" if markdown.len() > 1 => {
                // Several Markdown roots: the engine names each root's folders
                // `<source name>/<path>` (the root itself is `<source name>`).
                let (first, rest) = selector.split_once('/').unwrap_or((selector, ""));
                if markdown
                    .iter()
                    .any(|source| source.name.eq_ignore_ascii_case(first))
                {
                    ensure!(
                        rest.is_empty() || !folder_is_excluded(rest, &vault.exclusions),
                        "reading {} is excluded",
                        reading.entity
                    );
                } else {
                    ensure!(
                        sqlite_folders,
                        "folder {selector:?} in workspace {:?} must start with the name of one of its Markdown sources ({}), e.g. \"{}/{selector}\"",
                        workspace.name,
                        markdown
                            .iter()
                            .map(|source| source.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                        markdown[0].name
                    );
                }
            }
            "folder" => {
                ensure!(
                    !markdown.is_empty() || !sqlite_sources.is_empty(),
                    "workspace reading kind folder has no declared source in workspace {:?}",
                    workspace.name
                );
                ensure!(
                    !folder_is_excluded(selector, &vault.exclusions),
                    "reading {} is excluded",
                    reading.entity
                );
            }
            "link" if !markdown.is_empty() || !sqlite_sources.is_empty() => {}
            "tag" | "log" if !markdown.is_empty() => {}
            _ => bail!(
                "workspace reading kind {kind} has no declared source in workspace {:?}",
                workspace.name
            ),
        }
    }
    Ok(vault)
}
/// Resolve files as one namespace. Identical reusable profiles may be shared; conflicting declarations are errors.
pub fn resolve(programs: Vec<Program>, user_home: &Path) -> Result<Program> {
    let mut all = Program::default();
    let mut defaults = Learning::default();
    let mut global_retrieval = None;
    for p in &programs {
        for (name, profile) in &p.profiles {
            if let Some(existing) = all.profiles.get(name) {
                ensure!(
                    existing == profile,
                    "conflicting profile {name} across config files"
                );
            } else {
                all.profiles.insert(name.clone(), profile.clone());
            }
        }
        merge_learning(&mut defaults, &p.learning)?;
        merge_settings(&mut all.settings, &p.settings)?;
        if let Some(r) = &p.retrieval {
            ensure!(
                global_retrieval.is_none(),
                "duplicate global retrieval policy"
            );
            global_retrieval = Some(r.clone());
        }
    }
    defaults = defaults.over(&Learning::default());
    let mut paths = std::collections::BTreeSet::new();
    for p in programs {
        let mut scopes = p.vaults;
        for mut workspace in p.workspaces {
            for source in &mut workspace.sources {
                match source {
                    Source::Sqlite(source) => {
                        source.db = expand(&source.db, user_home);
                        ensure!(
                            Path::new(&source.db).is_absolute(),
                            "SQLite database path must be absolute: {}",
                            source.db
                        );
                    }
                    Source::Markdown(source) => {
                        source.path = expand(&source.path, user_home);
                        ensure!(
                            Path::new(&source.path).is_absolute(),
                            "Markdown path must be absolute: {}",
                            source.path
                        );
                        source.path = std::fs::canonicalize(&source.path)
                            .unwrap_or_else(|_| PathBuf::from(&source.path))
                            .to_string_lossy()
                            .into_owned();
                    }
                    Source::Arena(_) | Source::Host(_) => {}
                }
            }
            scopes.push(lower_workspace(workspace.clone())?);
            all.workspaces.push(workspace);
        }
        for mut v in scopes {
            if !v.path.starts_with("workspace:") {
                v.path = expand(&v.path, user_home);
                ensure!(
                    Path::new(&v.path).is_absolute(),
                    "vault path must be absolute: {}",
                    v.path
                );
                let canonical =
                    std::fs::canonicalize(&v.path).unwrap_or_else(|_| PathBuf::from(&v.path));
                v.path = canonical.to_string_lossy().into_owned();
            }
            ensure!(paths.insert(v.path.clone()), "duplicate vault {}", v.path);
            v.profiles = all.profiles.clone();
            v.learning = v.learning.over(&defaults);
            v.retrieval = v.retrieval.or_else(|| global_retrieval.clone());
            let mut entities = std::collections::BTreeSet::new();
            for r in &mut v.readings {
                if r.definition.is_some() && r.profile == "inline" {
                    r.profile = format!("inline:{}", r.entity);
                }
                let key = if r.pattern {
                    format!("matching {}", r.entity.to_lowercase())
                } else {
                    r.entity.to_lowercase()
                };
                ensure!(entities.insert(key), "duplicate reading {}", r.entity);
                r.learning = r.learning.over(&v.learning);
                if r.definition.is_none() {
                    if let Some(def) = all.profiles.get(&r.profile) {
                        r.definition = Some(def.clone());
                    } else if let Some(key) = builtin(&r.profile) {
                        r.profile = key.into();
                    } else {
                        bail!("unknown profile {}", r.profile)
                    }
                }
                // Named workspaces check folder exclusions per Markdown root
                // while lowering; a path-keyed vault has exactly one root.
                let (kind, name) = split_entity(&r.entity);
                ensure!(
                    kind != "folder"
                        || v.path.starts_with("workspace:")
                        || !folder_is_excluded(name, &v.exclusions),
                    "reading {} is excluded",
                    r.entity
                );
            }
            for policy in &mut v.note_policies {
                if policy.root.is_none() {
                    policy.root = Some(v.path.clone());
                }
            }
            v.targets = v.targets.iter().map(|t| expand(t, user_home)).collect();
            all.vaults.push(v);
        }
    }
    all.learning = defaults;
    all.retrieval = global_retrieval;
    Ok(all)
}
fn merge_learning(a: &mut Learning, b: &Learning) -> Result<()> {
    macro_rules! m {
        ($f:ident) => {
            if b.$f.is_some() {
                ensure!(a.$f.is_none(), concat!("duplicate global ", stringify!($f)));
                a.$f = b.$f.clone();
            }
        };
    }
    m!(as_of);
    m!(selection);
    m!(budget);
    m!(sample);
    m!(favor);
    m!(periods);
    Ok(())
}
fn merge_settings(a: &mut Settings, b: &Settings) -> Result<()> {
    macro_rules! m {
        ($f:ident) => {
            if b.$f.is_some() {
                ensure!(
                    a.$f.is_none(),
                    concat!("duplicate setting ", stringify!($f))
                );
                a.$f = b.$f.clone();
            }
        };
    }
    m!(generation);
    m!(local_model);
    m!(updates);
    m!(embedding_limit);
    m!(minimum_tags);
    m!(minimum_links);
    m!(minimum_folders);
    m!(total_limit);
    m!(format);
    Ok(())
}
/// Non-recursive .enzyme files form the reusable profile namespace. No includes or code execution.
pub fn load_directory(directory: &Path, user_home: &Path) -> Result<Option<Program>> {
    if !directory.exists() {
        return Ok(None);
    }
    let mut paths = std::fs::read_dir(directory)?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    paths.retain(|p| p.extension().is_some_and(|e| e == "enzyme") && p.is_file());
    paths.sort();
    if paths.is_empty() {
        return Ok(None);
    }
    let programs = paths
        .iter()
        .map(|p| {
            parse(&std::fs::read_to_string(p)?)
                .with_context(|| format!("invalid reading config {}", p.display()))
        })
        .collect::<Result<Vec<_>>>()?;
    resolve(programs, user_home).map(Some)
}

pub fn instructions(vault: &Vault) -> String {
    let mut s = format!(
        "# Enzyme reading instructions: {}\n\nUse this vault as the Enzyme working directory. Quoted policies are agent guidance, not installed hooks.\n",
        vault.path
    );
    if let Some(steps) = &vault.retrieval {
        s.push_str("\n## When context is needed\n");
        for step in steps {
            s.push_str(&format!("- {step}\n"));
        }
    }
    for m in &vault.memories {
        s.push_str(&format!("\n## Remember in {}\n\nConsider appending when any of these conditions is supported:\n",m.destination));
        for x in &m.conditions {
            s.push_str(&format!("- {x}\n"))
        }
        s.push_str("\nWhen appending:\n");
        for x in &m.guidance {
            s.push_str(&format!("- {x}\n"))
        }
        s.push_str("- Follow current user instructions and host write permissions. Append dated, source-linked observations without overwriting history.\n- After writing, run `enzyme refresh --quiet` so the material enters retrieval.\n");
    }
    for policy in &vault.note_policies {
        let place = match (policy.resolved_folder(), &policy.source) {
            (Some(folder), _) => folder.to_string_lossy().into_owned(),
            (None, Some(source)) => {
                format!("{} in Markdown source {}", policy.folder, quote(source))
            }
            (None, None) => policy.folder.clone(),
        };
        s.push_str(&format!("\n## Create notes in {place}\n\n"));
        if policy.conditions.is_empty() {
            s.push_str("New notes belong in this folder.\n");
        } else {
            s.push_str("Consider creating a new note when any of these conditions is supported:\n");
            for x in &policy.conditions {
                s.push_str(&format!("- {x}\n"));
            }
        }
        s.push_str("\nWhen creating a note:\n");
        for x in &policy.guidance {
            s.push_str(&format!("- {x}\n"));
        }
        s.push_str("- Follow current user instructions and host write permissions. Create new Markdown files only in this folder; never overwrite or move existing notes.\n- After writing, run `enzyme refresh --quiet` so the material enters retrieval.\n");
    }
    for t in &vault.targets {
        s.push_str(&format!("\nExternal target: {t}. Use `enzyme catalyze --target` when relevant; target-only themes may be missed.\n"))
    }
    s
}
/// Lower source declarations to the existing config structure; runtime-only fields stay typed.
pub fn config_value(p: &Program) -> serde_json::Value {
    use serde_json::{Map, Value, json};
    let mut root = Map::new();
    let mut defaults = Map::new();
    macro_rules! setting {
        ($field:ident,$key:expr) => {
            if let Some(v) = &p.settings.$field {
                defaults.insert($key.into(), json!(v));
            }
        };
    }
    setting!(embedding_limit, "max_embedding_files");
    setting!(minimum_tags, "minimum_tags");
    setting!(minimum_links, "minimum_links");
    setting!(minimum_folders, "minimum_folders");
    setting!(total_limit, "total_limit");
    setting!(format, "catalyst_format");
    root.insert("defaults".into(), Value::Object(defaults));
    let mut llm = Map::new();
    if let Some(v) = &p.settings.generation {
        llm.insert("mode".into(), json!(v));
    }
    if let Some(v) = &p.settings.local_model {
        llm.insert("local_model".into(), json!(v));
    }
    root.insert("llm".into(), Value::Object(llm));
    if let Some(v) = p.settings.updates {
        root.insert("update".into(), json!({"auto":v}));
    }
    let mut vaults = Map::new();
    for v in &p.vaults {
        // Name patterns have no single entity; they expand at selection time.
        let entities: Vec<Value> = v
            .readings
            .iter()
            .filter(|r| !r.pattern)
            .map(|r| {
                if r.profile == "auto" {
                    json!(r.entity)
                } else {
                    let mut m = Map::new();
                    m.insert(
                        r.entity.clone(),
                        json!({"profile":r.profile,"expandable":r.include_linked_pages}),
                    );
                    Value::Object(m)
                }
            })
            .collect();
        let mut item = json!({"entities":entities,"excluded_folders":v.exclusions,"excluded_tags":v.excluded_tags,"excluded_links":v.excluded_links,"frontmatter_link_fields":v.frontmatter_link_fields,"targets":v.targets});
        if let Some(b) = v.learning.budget {
            item["min_top_catalysts"] = json!(b);
        }
        if let Some(n) = v.embedding_limit {
            item["max_embedding_files"] = json!(n);
        }
        vaults.insert(v.path.clone(), item);
    }
    root.insert("vaults".into(), Value::Object(vaults));
    let mut workspaces = Map::new();
    for workspace in &p.workspaces {
        if workspace.markdown_path().is_some() {
            // A path-addressed Markdown workspace keeps the legacy runtime and index identity.
            continue;
        }
        let mut sources = Map::new();
        for source in &workspace.sources {
            if let Source::Markdown(source) = source {
                // Matches NotesSourceToml. Folder exclusions are workspace-wide
                // (`excluded_folders`), which the engine applies in every root.
                sources.insert(
                    source.name.clone(),
                    json!({
                        "path": source.path,
                        "writable": workspace.is_writable_markdown(&source.name),
                    }),
                );
            }
            if let Source::Sqlite(source) = source {
                let who = match &source.who {
                    SqliteWho::Columns { columns } => json!(columns),
                    SqliteWho::JsonArray { column } => {
                        json!({"format":"json_array", "column":column})
                    }
                    SqliteWho::Delimited { column, delimiter } => {
                        json!({"format":"delimited", "column":column, "delimiter":delimiter})
                    }
                };
                let mut roles = json!({
                    "id": source.id,
                    "who": who,
                    "when": source.when,
                    "what": source.what,
                    "where": source.where_columns,
                });
                if let Some(column) = &source.document_ref {
                    roles["document_ref"] = json!(column);
                }
                if let Some(column) = &source.weight {
                    roles["weight"] = json!(column);
                }
                // These names match SqliteSourceConfig exactly.
                let mut timestamp = json!({"unit": source.timestamp_unit});
                if let Some(epoch) = &source.timestamp_epoch {
                    timestamp["epoch"] = json!(epoch);
                }
                let mut config = json!({
                    "db": source.db,
                    "query": source.query,
                    "roles": roles,
                    "timestamp": timestamp,
                });
                if let Some(filter) = &source.filter {
                    config["filter"] = json!(filter);
                }
                sources.insert(source.name.clone(), config);
            }
        }
        let resolved = p
            .vaults
            .iter()
            .find(|vault| vault.path == format!("workspace:{}", workspace.name));
        let entities: Vec<Value> = resolved.into_iter().flat_map(|vault| vault.readings.iter())
            .filter(|r| !r.pattern)
            .map(|r| {
                if r.profile == "auto" { json!(r.entity) }
                else { json!({r.entity.clone(): {"profile": r.profile, "expandable": r.include_linked_pages}}) }
            }).collect();
        let mut item =
            json!({"sources": sources, "entities": entities, "targets": workspace.targets});
        if let Some(budget) = workspace.learning.budget {
            item["min_top_catalysts"] = json!(budget);
        }
        if let Some(format) = &workspace.format {
            item["catalyst_format"] = json!(format);
        }
        // These names match WorkspaceTomlSection; empty lists are omitted so
        // source-only workspaces lower exactly as before.
        for (key, list) in [
            ("excluded_folders", &workspace.exclusions),
            ("excluded_tags", &workspace.excluded_tags),
            ("excluded_links", &workspace.excluded_links),
            ("frontmatter_link_fields", &workspace.frontmatter_link_fields),
        ] {
            if !list.is_empty() {
                item[key] = json!(list);
            }
        }
        if let Some(limit) = workspace.embedding_limit {
            item["max_embedding_files"] = json!(limit);
        }
        workspaces.insert(workspace.name.clone(), item);
    }
    root.insert("workspaces".into(), Value::Object(workspaces));
    Value::Object(root)
}
fn quote(s: &str) -> String {
    serde_json::to_string(s).unwrap()
}
/// Longest element that still reads as a short scalar token rather than prose.
const SHORT_ELEMENT: usize = 30;
/// Column budget a filled inline list tries to stay within.
const LINE_BUDGET: usize = 80;

/// Lists render by element shape, not by which construct holds them. Short scalar
/// tokens (exclusions, reference fields, terse profile guidance) fill inline across
/// continuation lines; prose elements keep one per line so they stay legible.
/// Render `{indent}{label}<list>`. The label is part of the signature because a
/// filled list must know the column its opening bracket actually lands on.
fn render_clause(label: &str, items: &[String], indent: &str) -> String {
    let prefix = format!("{indent}{label}");
    if items
        .iter()
        .all(|i| i.chars().count() <= SHORT_ELEMENT && !i.contains('\n'))
    {
        return format!(
            "{prefix}{}",
            render_filled_list(items, indent, prefix.len())
        );
    }
    let mut s = prefix;
    s.push_str("{\n");
    for item in items {
        s.push_str(&format!("{indent}  {}\n", quote(item)));
    }
    s.push_str(&format!("{indent}}}"));
    s
}

/// Fill quoted items across as few lines as the budget allows. A long list wraps
/// rather than falling back to a block, so exclusions stay visually compact.
fn render_filled_list(items: &[String], indent: &str, start_column: usize) -> String {
    let continuation = format!("{indent}  ");
    let mut s = String::from("[");
    let mut column = start_column + 1;
    for (i, item) in items.iter().enumerate() {
        let quoted = quote(item);
        let separator = if i == 0 { "" } else { "," };
        if i > 0 && column + separator.len() + 1 + quoted.chars().count() > LINE_BUDGET {
            s.push_str(separator);
            s.push('\n');
            s.push_str(&continuation);
            column = continuation.len();
        } else {
            s.push_str(separator);
            if i > 0 {
                s.push(' ');
                column += 1;
            }
            column += separator.len();
        }
        s.push_str(&quoted);
        column += quoted.chars().count();
    }
    s.push(']');
    s
}
pub fn render_profile(name: &str, p: &Profile) -> String {
    let mut s = format!(
        "profile {name} {{\n  seek {}\n{}\n",
        quote(&p.seek),
        render_clause("notice ", &p.notice, "  ")
    );
    if !p.ask.is_empty() {
        s.push_str(&format!("{}\n", render_clause("ask ", &p.ask, "  ")));
    }
    if let Some(n) = &p.note {
        s.push_str(&format!("  note {}\n", quote(n)));
    }
    if !p.recognize.is_empty() {
        s.push_str(&format!(
            "{}\n",
            render_clause("recognize ", &p.recognize, "  ")
        ));
    }
    s.push_str("}\n\n");
    s
}
pub fn render_vault(v: &Vault) -> String {
    format!("vault {} {{\n{}}}\n", quote(&v.path), render_body(v))
}

/// Render the statements inside a `vault` or `workspace` block, each line
/// indented by two spaces. Accepts both unresolved input and effective specs.
fn render_body(v: &Vault) -> String {
    let mut normalized = v.clone();
    normalized.learning = v.learning.over(&Learning::default());
    for r in &mut normalized.readings {
        r.learning = r.learning.over(&normalized.learning);
    }
    let v = &normalized;
    let mut s = String::new();
    if let Some(n) = v.embedding_limit {
        s.push_str(&format!(
            "  prepare up to {n} documents per run newest first\n"
        ));
    }
    s.push_str(&render_learning(&v.learning, "  "));
    if let Some(f) = &v.format {
        s.push_str(&format!(
            "  produce {}\n",
            match f.as_str() {
                "thesis" => "theses",
                "claim" => "claims",
                _ => "questions",
            }
        ));
    }
    // Separate the preamble from the readings only when there is a preamble; with
    // exclusions moved below, a vault now commonly opens straight on its readings.
    if !s.is_empty() {
        s.push('\n');
    }
    for r in &v.readings {
        let (k, n) = split_entity(&r.entity);
        if r.pattern {
            s.push_str(&format!(
                "  learn questions from {k}s matching {}",
                quote(n)
            ));
        } else {
            let selector = if k == "thread" {
                let (source, thread) = n.split_once('/').expect("parsed thread has source");
                format!("{} in source {}", quote(thread), quote(source))
            } else if k == "channel" && !n.is_empty() && n.chars().all(|ch| ch.is_ascii_digit())
            {
                n.to_string()
            } else {
                quote(n)
            };
            s.push_str(&format!("  learn questions from {k} {selector}"));
        }
        if r.include_linked_pages {
            s.push_str("\n    including linked pages")
        }
        if r.include_who_links {
            s.push_str("\n    including who links")
        }
        if let Some(p) = &r.definition {
            s.push_str(&format!(
                "\n    about {}",
                render_profile("", p)
                    .replacen("profile  {", "profile {", 1)
                    .trim_end()
            ));
        } else if r.profile != "auto" {
            s.push_str(&format!("\n    about {}", r.profile));
        }
        let local = Learning {
            // A vault-wide reference time never renders on a reading.
            as_of: None,
            selection: r
                .learning
                .selection
                .clone()
                .filter(|x| Some(x) != v.learning.selection.as_ref()),
            budget: r.learning.budget.filter(|n| Some(*n) != v.learning.budget),
            // Sample and preference are one surface policy, never field deltas.
            sample: (r.learning.sampling_mode() != v.learning.sampling_mode())
                .then(|| r.learning.sample.clone())
                .flatten(),
            favor: (r.learning.sampling_mode() != v.learning.sampling_mode())
                .then(|| r.learning.favor.clone())
                .flatten(),
            periods: r
                .learning
                .periods
                .filter(|p| Some(p) != v.learning.periods.as_ref()),
        };
        if local != Learning::default() {
            s.push_str(" {\n");
            s.push_str(&render_learning(&local, "      "));
            s.push_str("    }");
        }
        s.push('\n');
    }
    // Mechanical qualifiers follow the readings. What the vault is read *for* is the
    // point of the file; what it skips is bookkeeping and should not open it.
    let mut qualifiers = String::new();
    for (kind, list) in [
        ("folders", &v.exclusions),
        ("tags", &v.excluded_tags),
        ("links", &v.excluded_links),
    ] {
        if !list.is_empty() {
            qualifiers.push_str(&format!(
                "{}\n",
                render_clause(&format!("leave out {kind} "), list, "  ")
            ));
        }
    }
    if !v.frontmatter_link_fields.is_empty() {
        qualifiers.push_str(&format!(
            "{}\n",
            render_clause("references in fields ", &v.frontmatter_link_fields, "  ")
        ));
    }
    if !qualifiers.is_empty() {
        s.push('\n');
        s.push_str(&qualifiers);
    }
    for t in &v.targets {
        s.push_str(&format!("\n  project questions into {}\n", quote(t)));
    }
    for policy in &v.note_policies {
        s.push_str(&format!("\n  remember in folder {}", quote(&policy.folder)));
        if let Some(source) = &policy.source {
            s.push_str(&format!(" in source {}", quote(source)));
        }
        if !policy.conditions.is_empty() {
            s.push_str(" when {\n");
            for c in &policy.conditions {
                s.push_str(&format!("    {}\n", quote(c)));
            }
            s.push_str("  }");
        }
        s.push_str(" create note");
        if !policy.guidance.is_empty() {
            s.push_str(" {\n");
            for g in &policy.guidance {
                s.push_str(&format!("    {}\n", quote(g)));
            }
            s.push_str("  }");
        }
        s.push('\n');
    }
    for m in &v.memories {
        s.push_str(&format!(
            "\n  remember in {} when {{\n",
            quote(&m.destination)
        ));
        for c in &m.conditions {
            s.push_str(&format!("    {}\n", quote(c)));
        }
        s.push_str("    append observation {\n");
        for g in &m.guidance {
            s.push_str(&format!("      {}\n", quote(g)));
        }
        s.push_str("    }\n  }\n");
    }
    if let Some(r) = &v.retrieval {
        s.push_str("\n  when asked {\n");
        for t in r {
            s.push_str(&format!("    {}\n", quote(t)));
        }
        s.push_str("  }\n");
    }
    s
}

pub fn render_workspace(workspace: &Workspace) -> String {
    let mut output = format!("workspace {} {{\n", quote(&workspace.name));
    let rendered: Vec<String> = workspace.sources.iter().map(render_source).collect();
    for (index, source) in rendered.iter().enumerate() {
        output.push_str(source);
        // One-line sources stack; a block source is followed by a blank line,
        // as is the whole source list.
        let next_is_block = rendered
            .get(index + 1)
            .is_some_and(|next| next.lines().count() > 1);
        if source.lines().count() > 1 || next_is_block || index + 1 == rendered.len() {
            output.push('\n');
        }
    }
    output.push_str(&render_body(&workspace.unresolved_vault()));
    output.push_str("}\n");
    output
}

fn render_source(source: &Source) -> String {
    let mut output = String::new();
    match source {
        Source::Markdown(source) => output.push_str(&format!(
            "  source markdown {} {{ path {} }}\n",
            quote(&source.name),
            quote(&source.path)
        )),
        Source::Host(source) => {
            let header = format!("  source {} {}", source.kind, quote(&source.name));
            let fields: Vec<String> = source
                .fields
                .iter()
                .map(|field| format!("{} {}", field.key, render_host_value(&field.value)))
                .collect();
            match fields.as_slice() {
                [] => output.push_str(&format!("{header} {{}}\n")),
                [only] if !only.contains('\n') && header.len() + only.len() + 5 <= LINE_BUDGET => {
                    output.push_str(&format!("{header} {{ {only} }}\n"))
                }
                _ => {
                    output.push_str(&format!("{header} {{\n"));
                    for field in &fields {
                        output.push_str(&format!("    {}\n", field.replace('\n', "\n    ")));
                    }
                    output.push_str("  }\n");
                }
            }
        }
        Source::Arena(source) => {
            output.push_str(&format!("  source arena {} {{\n", quote(&source.name)));
            output.push_str("    channels {\n");
            for channel in &source.channels {
                output.push_str(&format!("      channel {}", channel.remote_id));
                if let Some(alias) = &channel.alias {
                    output.push_str(&format!(" as {}", quote(alias)));
                } else if let (Some(slug), Some(title)) = (&channel.slug, &channel.title) {
                    output.push_str(&format!(" slug {} title {}", quote(slug), quote(title)));
                }
                output.push('\n');
            }
            output.push_str("    }\n  }\n");
        }
        Source::Sqlite(source) => {
            output.push_str(&format!("  source sqlite {} {{\n", quote(&source.name)));
            output.push_str(&format!("    database {}\n", quote(&source.db)));
            if source.query.contains('\n') && !source.query.contains("\"\"\"") {
                output.push_str("    query \"\"\"");
                output.push_str(&source.query);
                output.push_str("\"\"\"\n");
            } else {
                output.push_str(&format!("    query {}\n", quote(&source.query)));
            }
            output.push_str(&format!("    id {}\n", render_columns(&source.id)));
            if let Some(column) = &source.document_ref {
                output.push_str(&format!("    document ref {}\n", quote(column)));
            }
            match &source.who {
                SqliteWho::Columns { columns } if !columns.is_empty() => {
                    output.push_str(&format!("    who {}\n", render_columns(columns)))
                }
                SqliteWho::JsonArray { column } => {
                    output.push_str(&format!("    who json_array {}\n", quote(column)))
                }
                SqliteWho::Delimited { column, delimiter } => output.push_str(&format!(
                    "    who delimited {} by {}\n",
                    quote(column),
                    quote(delimiter)
                )),
                _ => {}
            }
            output.push_str(&format!(
                "    when {} unit {}",
                quote(&source.when),
                source.timestamp_unit
            ));
            if let Some(epoch) = &source.timestamp_epoch {
                output.push_str(&format!(" epoch {}", quote(epoch)));
            }
            output.push('\n');
            output.push_str(&format!("    what {}\n", render_columns(&source.what)));
            if !source.where_columns.is_empty() {
                output.push_str(&format!(
                    "    where {}\n",
                    render_columns(&source.where_columns)
                ));
            }
            if let Some(column) = &source.weight {
                output.push_str(&format!("    weight {}\n", quote(column)));
            }
            if let Some(filter) = &source.filter {
                output.push_str(&format!("    filter {}\n", quote(filter)));
            }
            output.push_str("  }\n");
        }
    }
    output
}

fn render_host_value(value: &HostValue) -> String {
    match value {
        HostValue::Bool(value) => value.to_string(),
        HostValue::Integer(value) => value.to_string(),
        HostValue::Text(value) => quote(value),
        HostValue::List(items) => {
            let inline = format!(
                "{{ {} }}",
                items.iter().map(|item| quote(item)).collect::<Vec<_>>().join(" ")
            );
            if inline.len() <= LINE_BUDGET - 8 {
                inline
            } else {
                let mut block = String::from("{\n");
                for item in items {
                    block.push_str(&format!("  {}\n", quote(item)));
                }
                block.push('}');
                block
            }
        }
    }
}

fn render_columns(columns: &[String]) -> String {
    if columns.len() == 1 {
        quote(&columns[0])
    } else {
        format!(
            "[{}]",
            columns
                .iter()
                .map(|column| quote(column))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}
fn render_learning(l: &Learning, indent: &str) -> String {
    let mut s = String::new();
    match &l.as_of {
        Some(AsOf::Date { date }) => s.push_str(&format!("{indent}as of {}\n", quote(date))),
        Some(AsOf::LatestEvidence) => s.push_str(&format!("{indent}as of latest evidence\n")),
        None => {}
    }
    if let Some(selection) = &l.selection {
        let (amount, cap) = match selection.amount {
            SelectionAmount::Count { count } => (count.to_string(), None),
            SelectionAmount::Percent { percent, up_to } => (format!("{percent}%"), up_to),
        };
        let mode = match selection.by {
            SelectionMode::Frequency => "frequency",
            SelectionMode::Recency => "recency",
        };
        s.push_str(&format!("{indent}select {amount} by {mode}"));
        if let Some(cap) = cap {
            s.push_str(&format!(" up to {cap}"));
        }
        s.push('\n');
    }
    if let Some(n) = l.budget {
        s.push_str(&format!("{indent}question budget {n}\n"));
    }
    if let Some(mode) = l.sampling_mode() {
        s.push_str(&format!("{indent}sample by {mode}\n"));
    }
    if let Some(periods) = l.periods {
        s.push_str(&format!("{indent}periods {}\n", periods.as_str()));
    }
    s
}
pub fn render_program(p: &Program) -> String {
    let mut s = String::from("// Enzyme reading configuration\n\n");
    if p.settings != Settings::default() {
        s.push_str("settings {\n");
        if let Some(x) = &p.settings.generation {
            s.push_str(&format!("  generation {x}\n"));
        }
        if let Some(x) = &p.settings.local_model {
            s.push_str(&format!("  model {}\n", quote(x)));
        }
        if let Some(x) = p.settings.updates {
            s.push_str(&format!(
                "  updates {}\n",
                if x { "enabled" } else { "disabled" }
            ));
        }
        if let Some(x) = p.settings.embedding_limit {
            s.push_str(&format!("  embedding limit {x}\n"));
        }
        for (k, n) in [
            ("tags", p.settings.minimum_tags),
            ("links", p.settings.minimum_links),
            ("folders", p.settings.minimum_folders),
            ("limit", p.settings.total_limit),
        ] {
            if let Some(n) = n {
                s.push_str(&format!("  selection {k} {n}\n"));
            }
        }
        if let Some(f) = &p.settings.format {
            s.push_str(&format!("  format {f}\n"));
        }
        s.push_str("}\n\n");
    }
    for (name, profile) in &p.profiles {
        s.push_str(&render_profile(name, profile));
    }
    if p.learning != Learning::default() {
        s.push_str("learning {\n");
        s.push_str(&render_learning(&p.learning, "  "));
        s.push_str("}\n\n");
    }
    for workspace in &p.workspaces {
        let mut scoped = workspace.clone();
        scoped.learning = workspace.learning.over(&p.learning);
        s.push_str(&render_workspace(&scoped));
        s.push('\n');
    }
    for v in &p.vaults {
        let mut scoped = v.clone();
        scoped.learning = v.learning.over(&p.learning);
        s.push_str(&render_vault(&scoped));
        s.push('\n');
    }
    if let Some(r) = &p.retrieval {
        s.push_str("when asked {\n");
        for x in r {
            s.push_str(&format!("  {}\n", quote(x)));
        }
        s.push_str("}\n");
    }
    s
}
pub fn atomic_write(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    let parent = path.parent().context("config needs a parent directory")?;
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".enzyme-write-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}
pub fn source_files(directory: &Path) -> Result<Vec<(PathBuf, Program)>> {
    if !directory.exists() {
        return Ok(vec![]);
    }
    let mut files = std::fs::read_dir(directory)?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    files.retain(|p| p.is_file() && p.extension().is_some_and(|x| x == "enzyme"));
    files.sort();
    files
        .into_iter()
        .map(|p| {
            let ast = parse(&std::fs::read_to_string(&p)?)
                .with_context(|| format!("parse {}", p.display()))?;
            Ok((p, ast))
        })
        .collect()
}

/// UI adapter; parsing and resolution are the same Rust functions used by init/refresh.
pub fn playground(source: &str) -> serde_json::Value {
    use serde_json::json;
    let result = (|| -> Result<(Program, Program)> {
        let ast = parse(source)?;
        ensure!(
            !ast.vaults.is_empty() || !ast.workspaces.is_empty(),
            "add a workspace or vault block to compile a reading plan"
        );
        let resolved = resolve(vec![ast.clone()], Path::new("/home/you"))?;
        Ok((ast, resolved))
    })();
    match result {
        Err(error) => {
            let message = format!("{error:#}");
            let mut parts = message.split(':');
            let line = parts
                .next()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(1);
            let column = parts
                .next()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(1);
            json!({"ok":false,"configReady":false,"diagnostics":[{"severity":"error","code":"syntax","message":message,"line":line,"column":column,"blocksConfig":true}],"plan":null,"instructions":"","trace":[]})
        }
        Ok((ast, p)) => {
            let mut trace = vec![];
            let mut vaults = vec![];
            for v in &p.vaults {
                let readings:Vec<_>=v.readings.iter().map(|r|{trace.push(json!({"line":source.lines().position(|l|l.contains(&format!("\"{}\"",split_entity(&r.entity).1))).unwrap_or(0)+1,"title":r.entity}));json!({"entity":r.entity,"profile":if let Some(def)=&r.definition{json!({"kind":"custom","name":r.profile,"definition":def})}else{json!({"kind":if r.profile=="auto"{"auto"}else{"builtin"},"key":r.profile})},"learning":{"budget":r.learning.budget.unwrap_or(15),"sample":r.learning.sample,"favor":r.learning.favor,"sampling_mode":r.learning.sampling_mode(),"periods":r.learning.periods,"selection":r.learning.selection},"includeLinkedPages":r.include_linked_pages,"includeWhoLinks":r.include_who_links,"pattern":r.pattern})}).collect();
                vaults.push(json!({"path":v.path,"learning":{"as_of":v.learning.as_of,"budget":v.learning.budget.unwrap_or(15),"sample":v.learning.sample,"favor":v.learning.favor,"sampling_mode":v.learning.sampling_mode(),"periods":v.learning.periods,"selection":v.learning.selection},"readings":readings,"memories":v.memories,"targets":v.targets,"retrieval":v.retrieval.as_ref().map(|steps|steps.iter().map(|text|json!({"type":"guidance","text":text})).collect::<Vec<_>>())}));
            }
            json!({"ok":true,"configReady":true,"diagnostics":[],"plan":{"version":"0.2","profiles":p.profiles,"defaults":p.learning,"vaults":vaults},"runtimePlan":p,"configSource":render_program(&ast),"instructions":p.vaults.iter().map(instructions).collect::<Vec<_>>().join("\n"),"trace":trace})
        }
    }
}

/// Tooling and build directories the index gate always excludes, regardless of
/// configuration. [`crate::document::discovery`]-equivalent defaults in
/// `enzyme-core` are built from this list, and `with_excluded_folders` appends to
/// rather than replaces them, so naming one of these in a program is a no-op.
///
/// Generated programs therefore omit them: a reader should see the vault's own
/// choices, not universal junk. Folders that a scan excludes but the index gate
/// does not enforce (`templates`, `.aside`) are real choices and stay explicit.
pub const IMPLICIT_EXCLUSIONS: &[&str] = &[
    ".agents",
    ".claude",
    ".codex",
    ".codex-work",
    ".conversations",
    ".enzyme",
    ".enzyme-embeddings",
    ".git",
    ".hermes",
    ".local",
    ".obsidian",
    ".pi",
    ".trash",
    "__pycache__",
    "build",
    "dist",
    "node_modules",
    "target",
];

/// Whether the index gate already excludes `name` without being told to.
pub fn is_implicit_exclusion(name: &str) -> bool {
    let name = name.trim().trim_matches('/');
    IMPLICIT_EXCLUSIONS
        .iter()
        .any(|d| d.eq_ignore_ascii_case(name))
}
