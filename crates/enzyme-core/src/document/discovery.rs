use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Context, Result};
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;

use crate::config::NotesRootConfig;

/// A discovered Markdown file with stable text provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredFile {
    pub root_name: String,
    pub root_path: PathBuf,
    pub relative_path: PathBuf,
    /// DB identity. An explicit document prefix is stable across root-count and
    /// display-name changes; roots without one preserve historical behavior.
    pub source_ref: String,
    pub writable: bool,
}

/// Escape glob metacharacters so a user-supplied folder name is matched
/// literally — both so `exclude: [notes[draft]]` matches that exact folder and
/// so a pathological name (e.g. an unbalanced `[`) can never produce an invalid
/// glob that aborts the whole index walk.
fn escape_glob_meta(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    for ch in name.chars() {
        if matches!(ch, '*' | '?' | '[' | ']' | '{' | '}' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Discovers markdown files within a vault directory, respecting include/exclude
/// patterns and `.gitignore` rules.
///
/// This is the **index gate** in the attention model
/// (`docs/design/attention-and-exclusions.md`): anything excluded here is never
/// read, so it produces no documents and no entities — invisible to embedding,
/// search, and catalyst selection alike. "Never index" strictly implies "never
/// surface," so the downstream surface gate ([`crate::pipeline::exclusions`])
/// need only name folders that *are* indexed but must not be catalyzed.
///
/// The defaults below cover tooling/system dirs. The user's `exclude` list
/// (`config.excluded_folders`) is layered on via [`Self::with_excluded_folders`]
/// (threaded from [`crate::pipeline::indexing::IndexingPipeline`]), which is what
/// makes `exclude` mean "leave it alone" (gone), not merely "don't surface."
pub struct FileDiscovery {
    vault_path: PathBuf,
    notes_roots: Option<Vec<NotesRootConfig>>,
    include_patterns: Vec<String>,
    exclude_patterns: Vec<String>,
}

impl FileDiscovery {
    /// Create a new `FileDiscovery` with default patterns.
    ///
    /// Defaults:
    /// - include: `["**/*.md"]`
    /// - exclude: `[".enzyme/**", ".obsidian/**", ".git/**", "node_modules/**"]`
    pub fn new(vault_path: PathBuf) -> Self {
        Self {
            vault_path,
            notes_roots: None,
            include_patterns: vec!["**/*.md".to_string()],
            exclude_patterns: vec![
                ".agents/**".to_string(),
                ".claude/**".to_string(),
                ".codex/**".to_string(),
                ".codex-work/**".to_string(),
                ".conversations/**".to_string(),
                ".enzyme/**".to_string(),
                ".enzyme-embeddings/**".to_string(),
                ".git/**".to_string(),
                ".hermes/**".to_string(),
                ".local/**".to_string(),
                ".obsidian/**".to_string(),
                ".pi/**".to_string(),
                ".trash/**".to_string(),
                "__pycache__/**".to_string(),
                "build/**".to_string(),
                "dist/**".to_string(),
                "node_modules/**".to_string(),
                "target/**".to_string(),
                "ENZYME_GUIDE.md".to_string(),
                "enzyme-config.yaml".to_string(),
            ],
        }
    }

    /// Create discovery over public, named Markdown roots.
    pub fn from_notes_roots(roots: Vec<NotesRootConfig>) -> Self {
        let fallback = roots
            .first()
            .map(|root| root.path.clone())
            .unwrap_or_else(|| PathBuf::from("."));
        let mut discovery = Self::new(fallback);
        discovery.notes_roots = Some(roots);
        discovery
    }

    /// Override the default include patterns.
    pub fn with_include_patterns(mut self, patterns: Vec<String>) -> Self {
        self.include_patterns = patterns;
        self
    }

    /// Override the default exclude patterns.
    pub fn with_exclude_patterns(mut self, patterns: Vec<String>) -> Self {
        self.exclude_patterns = patterns;
        self
    }

    /// Extend the index-gate excludes with the user's "leave it alone" folders.
    ///
    /// This is what makes `config.excluded_folders` mean **don't index** (gone),
    /// rather than merely "don't surface" — the behavior the attention model
    /// (`docs/design/attention-and-exclusions.md`) commits to. Each name becomes a
    /// vault-root-anchored `{name}/**` glob, matching how the built-in defaults are
    /// written, and is appended to (never replaces) the defaults.
    ///
    /// Matching is case-insensitive (see [`Self::discover`]), so the index gate
    /// agrees with the lowercased entity-level surface gate
    /// ([`crate::pipeline::exclusions::FolderExclusions`]): `exclude: [Archive]`
    /// excludes an on-disk `archive/` too. Glob metacharacters in a name are
    /// escaped so it matches literally and a pathological name can never abort the
    /// index walk. A `.` (vault root) is skipped: "don't surface the root as a
    /// topic" is a surface-gate concept and must never mean "index nothing."
    pub fn with_excluded_folders<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for name in names {
            let trimmed = name.as_ref().trim().trim_matches('/');
            if trimmed.is_empty() || trimmed == "." {
                continue;
            }
            self.exclude_patterns
                .push(format!("{}/**", escape_glob_meta(trimmed)));
        }
        self
    }

    /// Walk the vault directory and return paths relative to the vault root.
    ///
    /// Uses the `ignore` crate, which respects `.gitignore` rules by default.
    pub fn discover(&self) -> Result<Vec<PathBuf>> {
        if self.notes_roots.is_some() {
            return Ok(self
                .discover_named()?
                .into_iter()
                .map(|file| PathBuf::from(file.source_ref))
                .collect());
        }
        self.walk_root(&self.vault_path, &[])
    }

    /// Walk every configured root with its own exclusions. Roots are sorted by
    /// name and files by source ref, making reconciliation deterministic.
    pub fn discover_named(&self) -> Result<Vec<DiscoveredFile>> {
        let roots = self.notes_roots.clone().unwrap_or_else(|| {
            vec![NotesRootConfig {
                name: "notes".to_string(),
                path: self.vault_path.clone(),
                document_ref_prefix: None,
                exclusions: Vec::new(),
                writable: true,
            }]
        });
        let multi = roots.len() > 1;
        let mut files = Vec::new();
        let mut explicit_prefixes = HashSet::new();
        for root in &roots {
            if let Some(prefix) = root.document_ref_prefix.as_deref() {
                validate_document_ref_prefix(prefix)?;
                anyhow::ensure!(
                    explicit_prefixes.insert(prefix),
                    "notes roots may not share document_ref_prefix {prefix:?}"
                );
            }
        }
        let mut source_refs = HashSet::new();
        for root in roots {
            anyhow::ensure!(
                !root.name.trim().is_empty(),
                "notes root name may not be empty"
            );
            anyhow::ensure!(
                !root.name.contains('/') && !root.name.contains('\\'),
                "notes root name may not contain path separators: {:?}",
                root.name
            );
            let canonical = root
                .path
                .canonicalize()
                .with_context(|| format!("notes root does not exist: {:?}", root.path))?;
            for relative_path in self.walk_root(&canonical, &root.exclusions)? {
                let relative = relative_path.to_string_lossy().replace('\\', "/");
                let source_ref = if let Some(prefix) = &root.document_ref_prefix {
                    format!("{prefix}/{relative}")
                } else if multi {
                    format!("{}/{relative}", root.name)
                } else {
                    relative
                };
                anyhow::ensure!(
                    source_refs.insert(source_ref.clone()),
                    "notes roots produced duplicate document ref {source_ref:?}"
                );
                files.push(DiscoveredFile {
                    root_name: root.name.clone(),
                    root_path: canonical.clone(),
                    relative_path,
                    source_ref,
                    writable: root.writable,
                });
            }
        }
        files.sort_by(|left, right| left.source_ref.cmp(&right.source_ref));
        Ok(files)
    }

    fn walk_root(
        &self,
        root: &std::path::Path,
        root_exclusions: &[String],
    ) -> Result<Vec<PathBuf>> {
        let vault_path = root
            .canonicalize()
            .with_context(|| format!("notes root does not exist: {:?}", root))?;

        let mut overrides = OverrideBuilder::new(&vault_path);
        // Case-insensitive so user `exclude` names match regardless of on-disk
        // casing and the index gate agrees with the lowercased surface gate.
        overrides
            .case_insensitive(true)
            .context("failed to enable case-insensitive matching")?;

        // Add include patterns (whitelisted globs).
        for pattern in &self.include_patterns {
            overrides
                .add(pattern)
                .with_context(|| format!("invalid include pattern: {pattern}"))?;
        }

        // Add exclude patterns (negated globs).
        for pattern in &self.exclude_patterns {
            overrides
                .add(&format!("!{pattern}"))
                .with_context(|| format!("invalid exclude pattern: {pattern}"))?;
        }
        for name in root_exclusions {
            let trimmed = name.trim().trim_matches('/');
            if trimmed.is_empty() || trimmed == "." {
                continue;
            }
            let pattern = format!("{}/**", escape_glob_meta(trimmed));
            overrides
                .add(&format!("!{pattern}"))
                .with_context(|| format!("invalid notes-root exclusion: {name}"))?;
        }

        let overrides = overrides
            .build()
            .context("failed to build override matcher")?;

        let walker = WalkBuilder::new(&vault_path)
            .overrides(overrides)
            .hidden(false) // don't skip dotfiles by default — our overrides handle it
            .build();

        let mut paths = Vec::new();
        for entry in walker {
            let entry = entry.context("error reading directory entry")?;
            if entry.file_type().is_some_and(|ft| ft.is_file()) {
                let relative = entry
                    .path()
                    .strip_prefix(&vault_path)
                    .unwrap_or(entry.path())
                    .to_path_buf();
                paths.push(relative);
            }
        }

        paths.sort();
        Ok(paths)
    }
}

fn validate_document_ref_prefix(prefix: &str) -> Result<()> {
    anyhow::ensure!(
        !prefix.trim().is_empty(),
        "notes root document_ref_prefix may not be empty"
    );
    anyhow::ensure!(
        prefix == prefix.trim(),
        "notes root document_ref_prefix may not have surrounding whitespace"
    );
    anyhow::ensure!(
        !prefix.contains('/') && !prefix.contains('\\'),
        "notes root document_ref_prefix must be one path segment: {prefix:?}"
    );
    anyhow::ensure!(
        !prefix.chars().any(char::is_control),
        "notes root document_ref_prefix may not contain control characters"
    );
    anyhow::ensure!(
        !prefix.starts_with("sqlite:"),
        "notes root document_ref_prefix may not use the reserved sqlite: namespace"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn create_test_vault() -> TempDir {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        // Markdown files that should be included
        fs::write(root.join("note1.md"), "# Note 1").unwrap();
        fs::create_dir_all(root.join("subfolder")).unwrap();
        fs::write(root.join("subfolder/note2.md"), "# Note 2").unwrap();
        fs::create_dir_all(root.join("deep/nested/folder")).unwrap();
        fs::write(root.join("deep/nested/folder/note3.md"), "# Note 3").unwrap();

        // Files that should be excluded
        fs::write(root.join("readme.txt"), "not markdown").unwrap();
        fs::write(root.join("image.png"), "binary").unwrap();
        fs::create_dir_all(root.join(".enzyme")).unwrap();
        fs::write(root.join(".enzyme/enzyme.db"), "database").unwrap();
        fs::create_dir_all(root.join(".obsidian")).unwrap();
        fs::write(root.join(".obsidian/app.json"), "{}").unwrap();
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::write(root.join("node_modules/pkg/readme.md"), "# pkg").unwrap();
        fs::create_dir_all(root.join(".agents/runs")).unwrap();
        fs::write(root.join(".agents/runs/transcript.md"), "# agent run").unwrap();
        fs::create_dir_all(root.join(".pi/cache")).unwrap();
        fs::write(root.join(".pi/cache/session.md"), "# pi session").unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::write(root.join("target/debug/build-log.md"), "# build log").unwrap();

        dir
    }

    #[test]
    fn test_discovers_markdown_files() {
        let vault = create_test_vault();
        let discovery = FileDiscovery::new(vault.path().to_path_buf());
        let files = discovery.discover().unwrap();

        assert_eq!(files.len(), 3);
        assert!(files.contains(&PathBuf::from("note1.md")));
        assert!(files.contains(&PathBuf::from("subfolder/note2.md")));
        assert!(files.contains(&PathBuf::from("deep/nested/folder/note3.md")));
    }

    #[test]
    fn named_roots_are_namespaced_and_apply_per_root_exclusions() {
        let first = TempDir::new().unwrap();
        let second = TempDir::new().unwrap();
        fs::write(first.path().join("same.md"), "# first").unwrap();
        fs::create_dir(first.path().join("private")).unwrap();
        fs::write(first.path().join("private/hidden.md"), "# hidden").unwrap();
        fs::write(second.path().join("same.md"), "# second").unwrap();

        let files = FileDiscovery::from_notes_roots(vec![
            NotesRootConfig {
                name: "primary".into(),
                path: first.path().to_path_buf(),
                document_ref_prefix: None,
                exclusions: vec!["private".into()],
                writable: true,
            },
            NotesRootConfig {
                name: "reference".into(),
                path: second.path().to_path_buf(),
                document_ref_prefix: None,
                exclusions: Vec::new(),
                writable: false,
            },
        ])
        .discover_named()
        .unwrap();

        assert_eq!(
            files
                .iter()
                .map(|file| file.source_ref.as_str())
                .collect::<Vec<_>>(),
            vec!["primary/same.md", "reference/same.md"]
        );
        assert!(files[0].writable);
        assert!(!files[1].writable);
    }

    #[test]
    fn explicit_document_prefix_is_stable_across_root_count_and_display_name() {
        let first = TempDir::new().unwrap();
        let second = TempDir::new().unwrap();
        fs::write(first.path().join("same.md"), "# first").unwrap();
        fs::write(second.path().join("same.md"), "# second").unwrap();

        let first_root = NotesRootConfig {
            name: "home".into(),
            path: first.path().to_path_buf(),
            document_ref_prefix: Some("md-home-stable".into()),
            exclusions: Vec::new(),
            writable: true,
        };
        let single = FileDiscovery::from_notes_roots(vec![first_root.clone()])
            .discover_named()
            .unwrap();
        assert_eq!(single[0].source_ref, "md-home-stable/same.md");

        let mut renamed = first_root.clone();
        renamed.name = "renamed-binding".into();
        let renamed_files = FileDiscovery::from_notes_roots(vec![renamed.clone()])
            .discover_named()
            .unwrap();
        assert_eq!(renamed_files[0].source_ref, single[0].source_ref);

        let multi = FileDiscovery::from_notes_roots(vec![
            renamed,
            NotesRootConfig {
                name: "reference".into(),
                path: second.path().to_path_buf(),
                document_ref_prefix: Some("md-reference-stable".into()),
                exclusions: Vec::new(),
                writable: false,
            },
        ])
        .discover_named()
        .unwrap();
        assert_eq!(
            multi
                .iter()
                .map(|file| file.source_ref.as_str())
                .collect::<Vec<_>>(),
            vec!["md-home-stable/same.md", "md-reference-stable/same.md"]
        );
    }

    #[test]
    fn duplicate_or_invalid_document_prefixes_fail_closed() {
        let first = TempDir::new().unwrap();
        let second = TempDir::new().unwrap();
        fs::write(first.path().join("a.md"), "# first").unwrap();
        fs::write(second.path().join("b.md"), "# second").unwrap();
        let root = |name: &str, path: &std::path::Path, prefix: &str| NotesRootConfig {
            name: name.into(),
            path: path.to_path_buf(),
            document_ref_prefix: Some(prefix.into()),
            exclusions: Vec::new(),
            writable: false,
        };

        let duplicate = FileDiscovery::from_notes_roots(vec![
            root("first", first.path(), "stable"),
            root("second", second.path(), "stable"),
        ])
        .discover_named()
        .unwrap_err();
        assert!(duplicate.to_string().contains("may not share"));

        for invalid in [
            "",
            " spaced",
            "nested/prefix",
            "nested\\prefix",
            "bad\nref",
            "sqlite:mail",
        ] {
            let error = FileDiscovery::from_notes_roots(vec![root("first", first.path(), invalid)])
                .discover_named()
                .unwrap_err();
            assert!(!error.to_string().is_empty(), "invalid prefix {invalid:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn configured_symlink_root_is_followed() {
        use std::os::unix::fs::symlink;
        let target = TempDir::new().unwrap();
        let holder = TempDir::new().unwrap();
        fs::write(target.path().join("note.md"), "# linked").unwrap();
        let link = holder.path().join("linked-notes");
        symlink(target.path(), &link).unwrap();
        let files = FileDiscovery::from_notes_roots(vec![NotesRootConfig {
            name: "notes".into(),
            path: link,
            document_ref_prefix: None,
            exclusions: Vec::new(),
            writable: true,
        }])
        .discover_named()
        .unwrap();
        assert_eq!(files[0].source_ref, "note.md");
    }

    /// The index gate: a user `exclude`d folder is not read at all.
    #[test]
    fn excluded_folder_is_not_indexed() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("projects")).unwrap();
        fs::write(root.join("projects/a.md"), "# a").unwrap();
        fs::create_dir_all(root.join("archive")).unwrap();
        fs::write(root.join("archive/old.md"), "# old").unwrap();

        let files = FileDiscovery::new(root.to_path_buf())
            .with_excluded_folders(["archive"])
            .discover()
            .unwrap();

        assert!(files.contains(&PathBuf::from("projects/a.md")));
        assert!(
            !files.iter().any(|p| p.starts_with("archive")),
            "excluded folder must not be indexed, got: {files:?}"
        );
    }

    /// The index gate must agree with the lowercased surface gate: an exclude
    /// entry whose casing differs from the on-disk folder still excludes it.
    #[test]
    fn excluded_folder_match_is_case_insensitive() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("Archive")).unwrap();
        fs::write(root.join("Archive/old.md"), "# old").unwrap();
        fs::write(root.join("keep.md"), "# keep").unwrap();

        let files = FileDiscovery::new(root.to_path_buf())
            .with_excluded_folders(["archive"]) // lowercase config vs `Archive/` dir
            .discover()
            .unwrap();

        assert!(files.contains(&PathBuf::from("keep.md")));
        assert!(
            !files.iter().any(|p| p.starts_with("Archive")),
            "case-mismatched exclude must still exclude, got: {files:?}"
        );
    }

    /// A folder name containing glob metacharacters is matched literally and never
    /// aborts the walk.
    #[test]
    fn excluded_folder_with_glob_metachars_is_literal_and_safe() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("notes[draft]")).unwrap();
        fs::write(root.join("notes[draft]/d.md"), "# d").unwrap();
        fs::create_dir_all(root.join("notesx")).unwrap();
        fs::write(root.join("notesx/y.md"), "# y").unwrap();

        // Also feed a pathological unbalanced-class name: must not error the walk.
        let files = FileDiscovery::new(root.to_path_buf())
            .with_excluded_folders(["notes[draft]", "bad["])
            .discover()
            .expect("pathological exclude name must not abort discovery");

        assert!(
            !files.iter().any(|p| p.starts_with("notes[draft]")),
            "literal metachar folder should be excluded, got: {files:?}"
        );
        assert!(
            files.contains(&PathBuf::from("notesx/y.md")),
            "the class-glob must not over-match a different folder, got: {files:?}"
        );
    }

    /// Two-gate guard: built-in *structural* folders (surface-gate only) must stay
    /// indexed. `exclude` feeds the index gate; the structural noise list does not.
    #[test]
    fn structural_folders_stay_indexed_by_default() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("templates")).unwrap();
        fs::write(root.join("templates/t.md"), "# tpl").unwrap();
        fs::create_dir_all(root.join("excalidraw")).unwrap();
        fs::write(root.join("excalidraw/d.md"), "# draw").unwrap();

        // No excludes configured: these are indexed (they're only suppressed later,
        // at the surface gate), even though they're structural noise as topics.
        let files = FileDiscovery::new(root.to_path_buf()).discover().unwrap();

        assert!(files.contains(&PathBuf::from("templates/t.md")));
        assert!(files.contains(&PathBuf::from("excalidraw/d.md")));
    }

    /// `.` (vault root) must never be turned into an index exclusion.
    #[test]
    fn root_and_blank_excludes_are_ignored_by_index_gate() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::write(root.join("note.md"), "# n").unwrap();

        let files = FileDiscovery::new(root.to_path_buf())
            .with_excluded_folders([".", "", "  "])
            .discover()
            .unwrap();

        assert!(
            files.contains(&PathBuf::from("note.md")),
            "root/blank excludes must not blank the vault, got: {files:?}"
        );
    }

    #[test]
    fn test_excludes_non_markdown() {
        let vault = create_test_vault();
        let discovery = FileDiscovery::new(vault.path().to_path_buf());
        let files = discovery.discover().unwrap();

        for path in &files {
            assert_eq!(
                path.extension().and_then(|e| e.to_str()),
                Some("md"),
                "non-markdown file found: {:?}",
                path
            );
        }
    }

    #[test]
    fn test_excludes_runtime_and_structural_dirs() {
        let vault = create_test_vault();
        let discovery = FileDiscovery::new(vault.path().to_path_buf());
        let files = discovery.discover().unwrap();

        for path in &files {
            let path_str = path.to_string_lossy();
            assert!(
                !path_str.starts_with(".enzyme"),
                "enzyme dir not excluded: {:?}",
                path
            );
            assert!(
                !path_str.starts_with(".obsidian"),
                "obsidian dir not excluded: {:?}",
                path
            );
            assert!(
                !path_str.starts_with("node_modules"),
                "node_modules not excluded: {:?}",
                path
            );
            assert!(
                !path_str.starts_with(".agents"),
                ".agents not excluded: {:?}",
                path
            );
            assert!(!path_str.starts_with(".pi"), ".pi not excluded: {:?}", path);
            assert!(
                !path_str.starts_with("target"),
                "target not excluded: {:?}",
                path
            );
        }
    }

    #[test]
    fn test_empty_vault() {
        let dir = TempDir::new().unwrap();
        let discovery = FileDiscovery::new(dir.path().to_path_buf());
        let files = discovery.discover().unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn test_returns_sorted_paths() {
        let vault = create_test_vault();
        let discovery = FileDiscovery::new(vault.path().to_path_buf());
        let files = discovery.discover().unwrap();

        let mut sorted = files.clone();
        sorted.sort();
        assert_eq!(files, sorted, "paths should be sorted");
    }

    #[test]
    fn test_paths_are_relative() {
        let vault = create_test_vault();
        let discovery = FileDiscovery::new(vault.path().to_path_buf());
        let files = discovery.discover().unwrap();

        for path in &files {
            assert!(path.is_relative(), "path should be relative: {:?}", path);
        }
    }

    #[test]
    fn test_invalid_vault_path() {
        let discovery = FileDiscovery::new(PathBuf::from("/nonexistent/vault/path"));
        let result = discovery.discover();
        assert!(result.is_err());
    }

    #[test]
    fn test_custom_include_patterns() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::write(root.join("note.md"), "md").unwrap();
        fs::write(root.join("note.txt"), "txt").unwrap();

        let discovery = FileDiscovery::new(root.to_path_buf())
            .with_include_patterns(vec!["**/*.txt".to_string()]);
        let files = discovery.discover().unwrap();

        assert_eq!(files.len(), 1);
        assert!(files.contains(&PathBuf::from("note.txt")));
    }
}
