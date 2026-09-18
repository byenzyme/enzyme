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
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Learning {
    pub budget: Option<usize>,
    pub sample: Option<String>,
    pub favor: Option<String>,
}
impl Learning {
    pub fn over(&self, base: &Self) -> Self {
        Self {
            budget: self.budget.or(base.budget),
            sample: self.sample.clone().or_else(|| base.sample.clone()),
            favor: self.favor.clone().or_else(|| base.favor.clone()),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Reading {
    pub entity: String,
    pub profile: String,
    pub definition: Option<Profile>,
    pub learning: Learning,
    pub include_linked_pages: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Memory {
    pub destination: String,
    pub conditions: Vec<String>,
    pub guidance: Vec<String>,
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
}
impl Vault {
    pub fn reading(&self, name: &str, kind: &str) -> Option<&Reading> {
        self.readings.iter().find(|r| {
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
    pub retrieval: Option<Vec<String>>,
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
        if c == '"' {
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
        } else if "{}[]=,".contains(c) {
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
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || "_-".contains(chars[i])) {
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
        self.need("[")?;
        let mut v = vec![];
        while !self.at("]") {
            v.push(self.string()?);
            if !self.eat(",") {
                break;
            }
        }
        self.need("]")?;
        ensure!(!v.is_empty(), "lists must not be empty");
        Ok(v)
    }
    fn set<T>(&self, place: &mut Option<T>, value: T) -> Result<()> {
        if place.is_some() {
            return self.err("setting is repeated in this scope");
        }
        *place = Some(value);
        Ok(())
    }
    fn learn_setting(&mut self, l: &mut Learning) -> Result<()> {
        if self.eat("sample") {
            self.need("across")?;
            self.need("time")?;
            self.set(&mut l.sample, "across_time".into())
        } else if self.eat("favor") {
            let value = if self.eat("recent") {
                self.need("periods")?;
                "recent"
            } else {
                self.need("all")?;
                self.need("periods")?;
                self.need("equally")?;
                "equal"
            };
            self.set(&mut l.favor, value.into())
        } else {
            self.need("question")?;
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
        self.need("learn")?;
        self.need("questions")?;
        self.need("from")?;
        let kind = self.word()?;
        ensure!(
            [
                "folder", "folders", "tag", "tags", "link", "links", "log", "logs"
            ]
            .contains(&kind.as_str()),
            "unknown reading kind {kind}"
        );
        let names = if kind.ends_with('s') {
            self.list()?
        } else {
            vec![self.string()?]
        };
        let kind = kind.trim_end_matches('s');
        let mut include = false;
        if self.eat("including") {
            self.need("linked")?;
            self.need("pages")?;
            include = true;
        }
        self.need("about")?;
        let (profile, definition) = if self.eat("profile") {
            ("inline".into(), Some(self.profile()?))
        } else {
            (self.word()?, None)
        };
        let mut learning = if self.at("{") {
            self.learning()?
        } else {
            Learning::default()
        };
        while ["sample", "favor", "question", "including"]
            .iter()
            .any(|s| self.at(s))
        {
            if self.eat("including") {
                ensure!(!include, "duplicate including linked pages");
                self.need("linked")?;
                self.need("pages")?;
                include = true;
            } else {
                self.learn_setting(&mut learning)?;
            }
        }
        ensure!(
            !include || kind == "folder",
            "only folders can include linked pages"
        );
        Ok(names
            .into_iter()
            .map(|name| Reading {
                entity: entity_selector(kind, &name),
                profile: profile.clone(),
                definition: definition.clone(),
                learning: learning.clone(),
                include_linked_pages: include,
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
        let mut v = Vault {
            path,
            ..Default::default()
        };
        let mut seen = std::collections::BTreeSet::new();
        while !self.at("}") {
            if self.at("learn") {
                v.readings.extend(self.reading()?);
            } else if self.eat("learning") {
                ensure!(seen.insert("learning"), "duplicate vault learning block");
                let l = self.learning()?;
                ensure!(
                    v.learning == Learning::default(),
                    "use a single learning block per scope"
                );
                v.learning = l;
            } else if ["sample", "favor", "question"].iter().any(|s| self.at(s)) {
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
                v.memories.push(self.memory()?);
            } else if self.at("when") {
                let r = self.retrieval()?;
                self.set(&mut v.retrieval, r)?;
            } else {
                return self.err(
                    "expected a reading, learning settings, exclusions, target, or agent policy",
                );
            }
        }
        self.need("}")?;
        Ok(v)
    }
}
pub fn parse(source: &str) -> Result<Program> {
    let mut p = Parser {
        tokens: lex(source)?,
        pos: 0,
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
        } else if p.at("when") {
            let x = p.retrieval()?;
            p.set(&mut out.retrieval, x)?;
        } else {
            return p.err("expected profile, let, settings, learning, vault, or when asked");
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
/// Resolve a group of files as one namespace. Duplicate declarations are errors.
pub fn resolve(programs: Vec<Program>, user_home: &Path) -> Result<Program> {
    let mut all = Program::default();
    let mut defaults = Learning::default();
    let mut global_retrieval = None;
    for p in &programs {
        for (name, profile) in &p.profiles {
            ensure!(
                all.profiles.insert(name.clone(), profile.clone()).is_none(),
                "duplicate profile {name} across config files"
            );
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
    let mut paths = std::collections::BTreeSet::new();
    for p in programs {
        for mut v in p.vaults {
            v.path = expand(&v.path, user_home);
            ensure!(
                Path::new(&v.path).is_absolute(),
                "vault path must be absolute: {}",
                v.path
            );
            let canonical =
                std::fs::canonicalize(&v.path).unwrap_or_else(|_| PathBuf::from(&v.path));
            v.path = canonical.to_string_lossy().into_owned();
            ensure!(paths.insert(v.path.clone()), "duplicate vault {}", v.path);
            v.profiles = all.profiles.clone();
            v.learning = v.learning.over(&defaults);
            v.retrieval = v.retrieval.or_else(|| global_retrieval.clone());
            let mut entities = std::collections::BTreeSet::new();
            for r in &mut v.readings {
                if r.definition.is_some() && r.profile == "inline" {
                    r.profile = format!("inline:{}", r.entity);
                }
                ensure!(
                    entities.insert(r.entity.to_lowercase()),
                    "duplicate reading {}",
                    r.entity
                );
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
                let (kind, name) = split_entity(&r.entity);
                ensure!(
                    kind != "folder"
                        || !v
                            .exclusions
                            .iter()
                            .any(|ex| name == ex || name.starts_with(&format!("{ex}/"))),
                    "reading {} is excluded",
                    r.entity
                );
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
    m!(budget);
    m!(sample);
    m!(favor);
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
        let entities: Vec<Value> = v
            .readings
            .iter()
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
    Value::Object(root)
}
fn quote(s: &str) -> String {
    serde_json::to_string(s).unwrap()
}
pub fn render_profile(name: &str, p: &Profile) -> String {
    let mut s = format!(
        "profile {name} {{\n  seek {}\n  notice {}\n",
        quote(&p.seek),
        serde_json::to_string(&p.notice).unwrap()
    );
    if !p.ask.is_empty() {
        s.push_str(&format!(
            "  ask {}\n",
            serde_json::to_string(&p.ask).unwrap()
        ));
    }
    if let Some(n) = &p.note {
        s.push_str(&format!("  note {}\n", quote(n)));
    }
    if !p.recognize.is_empty() {
        s.push_str(&format!(
            "  recognize {}\n",
            serde_json::to_string(&p.recognize).unwrap()
        ));
    }
    s.push_str("}\n\n");
    s
}
pub fn render_vault(v: &Vault) -> String {
    let mut s = format!("vault {} {{\n", quote(&v.path));
    for (kind, list) in [
        ("folders", &v.exclusions),
        ("tags", &v.excluded_tags),
        ("links", &v.excluded_links),
    ] {
        if !list.is_empty() {
            s.push_str(&format!(
                "  leave out {kind} {}\n",
                serde_json::to_string(list).unwrap()
            ));
        }
    }
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
    if !v.frontmatter_link_fields.is_empty() {
        s.push_str(&format!(
            "  references in fields {}\n",
            serde_json::to_string(&v.frontmatter_link_fields).unwrap()
        ));
    }
    s.push('\n');
    for r in &v.readings {
        let (k, n) = split_entity(&r.entity);
        s.push_str(&format!("  learn questions from {k} {}", quote(n)));
        if r.include_linked_pages {
            s.push_str("\n    including linked pages")
        }
        if let Some(p) = &r.definition {
            s.push_str(&format!(
                "\n    about {}",
                render_profile("", p)
                    .replacen("profile  {", "profile {", 1)
                    .trim_end()
            ));
        } else {
            s.push_str(&format!("\n    about {}", r.profile));
        }
        let local = Learning {
            budget: r.learning.budget.filter(|n| Some(*n) != v.learning.budget),
            sample: r
                .learning
                .sample
                .clone()
                .filter(|x| Some(x) != v.learning.sample.as_ref()),
            favor: r
                .learning
                .favor
                .clone()
                .filter(|x| Some(x) != v.learning.favor.as_ref()),
        };
        if local != Learning::default() {
            s.push_str(" {\n");
            s.push_str(&render_learning(&local, "      "));
            s.push_str("    }");
        }
        s.push('\n');
    }
    for t in &v.targets {
        s.push_str(&format!("\n  project questions into {}\n", quote(t)));
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
    s.push_str("}\n");
    s
}
fn render_learning(l: &Learning, indent: &str) -> String {
    let mut s = String::new();
    if let Some(n) = l.budget {
        s.push_str(&format!("{indent}question budget {n}\n"));
    }
    if l.sample.is_some() {
        s.push_str(&format!("{indent}sample across time\n"));
    }
    if let Some(f) = &l.favor {
        s.push_str(&format!(
            "{indent}favor {}\n",
            if f == "equal" {
                "all periods equally"
            } else {
                "recent periods"
            }
        ));
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
    for v in &p.vaults {
        s.push_str(&render_vault(v));
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
            !ast.vaults.is_empty(),
            "add a vault block to compile a reading plan"
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
                let readings:Vec<_>=v.readings.iter().map(|r|{trace.push(json!({"line":source.lines().position(|l|l.contains(&format!("\"{}\"",split_entity(&r.entity).1))).unwrap_or(0)+1,"title":r.entity}));json!({"entity":r.entity,"profile":if let Some(def)=&r.definition{json!({"kind":"custom","name":r.profile,"definition":def})}else{json!({"kind":if r.profile=="auto"{"auto"}else{"builtin"},"key":r.profile})},"learning":{"budget":r.learning.budget.unwrap_or(15),"sample":r.learning.sample,"favor":r.learning.favor},"includeLinkedPages":r.include_linked_pages})}).collect();
                vaults.push(json!({"path":v.path,"learning":{"budget":v.learning.budget.unwrap_or(15),"sample":v.learning.sample,"favor":v.learning.favor},"readings":readings,"memories":v.memories,"targets":v.targets,"retrieval":v.retrieval.as_ref().map(|steps|steps.iter().map(|text|json!({"type":"guidance","text":text})).collect::<Vec<_>>())}));
            }
            json!({"ok":true,"configReady":true,"diagnostics":[],"plan":{"version":"0.2","profiles":p.profiles,"defaults":p.learning,"vaults":vaults},"runtimePlan":p,"configSource":render_program(&ast),"instructions":p.vaults.iter().map(instructions).collect::<Vec<_>>().join("\n"),"trace":trace})
        }
    }
}
