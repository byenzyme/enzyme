//! Unified folder-exclusion source of truth.
//!
//! Historically, "which folders should be excluded" was answered by four
//! independent lists that drifted out of sync (see
//! `docs/design/entity-exclusions-refactor.md`):
//!
//! * `repository::Database::EXCLUDED_FOLDER_NAMES` (trending path, 9 entries)
//! * `entity_map::EXCLUDED_FOLDERS` (folding path, 10 entries — had `excalidraw`)
//! * `entity_map::GENERIC_FOLDERS` (surfacing suppression — a *different* concept)
//! * `EnzymeConfig::excluded_folders` (user config, only honored in the curated path)
//!
//! [`FolderExclusions`] is the single, normalized set that merges the built-in
//! **structural** defaults with the user's `config.excluded_folders`. It is the
//! type every *structural* consumer (trending, entity-map folding, curated
//! selection) should thread through.
//!
//! ## Scope of this type
//!
//! This type answers exactly one question: **"is this folder structural noise
//! that should never be treated as a top-level content entity?"** It does NOT
//! model:
//!
//! * `GENERIC_FOLDERS` (daily/notes/…): those are indexed and selectable, they
//!   are merely suppressed from being *surfaced as a topic*. That is a
//!   surfacing/ranking concern, kept deliberately separate here — see the design
//!   doc's "GENERIC_FOLDERS: merge or separate" decision.
//! * discovery-level exclusion (skipping indexing entirely): that is a distinct
//!   layer handled by [`crate::document::discovery::FileDiscovery`] glob
//!   patterns. See the design doc's "discovery vs selection layering" decision.

use std::collections::HashSet;

use crate::models::ROOT_FOLDER_ENTITY;

/// Built-in structural folders that should never be surfaced as top-level
/// content entities, regardless of user config.
///
/// This is the canonical union of the previous `EXCLUDED_FOLDERS` (which had
/// `excalidraw`) and `EXCLUDED_FOLDER_NAMES` (which did not). The user's
/// `config.excluded_folders` *extends* this set — it never replaces it.
pub const BUILTIN_STRUCTURAL_FOLDERS: &[&str] = &[
    ".claude",
    ".enzyme",
    ".git",
    ".obsidian",
    ".trash",
    "config",
    "excalidraw",
    "migrations",
    "templater",
    "templates",
];

/// A normalized set of folder names to exclude from structural selection.
///
/// Names are normalized exactly once, using the same rules as
/// [`crate::config::enzyme_config::parse_entity_ref`] applies to a
/// `folder:{x}` reference:
///
/// * trimmed of surrounding whitespace,
/// * lowercased,
/// * a literal `.` is mapped to [`ROOT_FOLDER_ENTITY`].
///
/// Membership tests via [`FolderExclusions::is_excluded`] normalize their input
/// the same way, so callers can pass raw entity names.
#[derive(Debug, Clone, Default)]
pub struct FolderExclusions {
    normalized: HashSet<String>,
}

impl FolderExclusions {
    /// Built-in structural defaults only (no user config).
    pub fn defaults() -> Self {
        Self::from_names(BUILTIN_STRUCTURAL_FOLDERS.iter().copied())
    }

    /// Built-in defaults merged with additional (e.g. user-config) folder names.
    ///
    /// This is the intended production constructor once config is threaded to a
    /// consumer: `FolderExclusions::with_extra(config.excluded_folders.iter())`.
    pub fn with_extra<I, S>(extra: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut set = Self::defaults();
        for name in extra {
            if let Some(n) = normalize_folder_name(name.as_ref()) {
                set.normalized.insert(n);
            }
        }
        set
    }

    /// Construct from an arbitrary iterator of raw names (built-ins NOT added).
    /// Primarily useful for tests.
    pub fn from_names<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let normalized = names
            .into_iter()
            .filter_map(|n| normalize_folder_name(n.as_ref()))
            .collect();
        Self { normalized }
    }

    /// Returns `true` if `name` (normalized) is an excluded folder.
    pub fn is_excluded(&self, name: &str) -> bool {
        match normalize_folder_name(name) {
            Some(n) => self.normalized.contains(&n),
            None => false,
        }
    }

    /// Number of distinct excluded folder names.
    pub fn len(&self) -> usize {
        self.normalized.len()
    }

    /// Whether the exclusion set is empty.
    pub fn is_empty(&self) -> bool {
        self.normalized.is_empty()
    }

    /// Iterate over the normalized excluded names.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.normalized.iter().map(String::as_str)
    }
}

/// Normalize a folder name identically to `parse_entity_ref("folder:{x}")`.
///
/// Returns `None` for empty (post-trim) names, matching the parser's behavior
/// of dropping empty references.
pub fn normalize_folder_name(raw: &str) -> Option<String> {
    let name = raw.trim().to_lowercase();
    if name.is_empty() {
        return None;
    }
    if name == "." {
        Some(ROOT_FOLDER_ENTITY.to_string())
    } else {
        Some(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_contain_union_of_legacy_lists() {
        let ex = FolderExclusions::defaults();
        // Present in both legacy lists.
        assert!(ex.is_excluded("templates"));
        assert!(ex.is_excluded(".claude"));
        // The historical drift: only in EXCLUDED_FOLDERS, missing from
        // EXCLUDED_FOLDER_NAMES. The unified set must include it.
        assert!(
            ex.is_excluded("excalidraw"),
            "excalidraw must be excluded in the unified defaults"
        );
    }

    #[test]
    fn membership_normalizes_case_and_whitespace() {
        let ex = FolderExclusions::defaults();
        assert!(ex.is_excluded("TEMPLATES"));
        assert!(ex.is_excluded("  Templates  "));
        assert!(ex.is_excluded(".Obsidian"));
    }

    #[test]
    fn with_extra_merges_defaults_and_config() {
        let ex = FolderExclusions::with_extra(["Archive", "  scratch  "]);
        // Built-ins still present.
        assert!(ex.is_excluded("excalidraw"));
        // Config entries added, normalized.
        assert!(ex.is_excluded("archive"));
        assert!(ex.is_excluded("scratch"));
    }

    #[test]
    fn empty_and_blank_names_are_dropped() {
        assert_eq!(normalize_folder_name(""), None);
        assert_eq!(normalize_folder_name("   "), None);
        let ex = FolderExclusions::with_extra(["", "   ", "keep"]);
        assert!(ex.is_excluded("keep"));
        assert!(!ex.is_excluded(""));
    }

    #[test]
    fn dot_maps_to_root_folder_entity() {
        assert_eq!(
            normalize_folder_name("."),
            Some(ROOT_FOLDER_ENTITY.to_string())
        );
        let ex = FolderExclusions::with_extra(["."]);
        assert!(ex.is_excluded("."));
        assert!(ex.is_excluded(ROOT_FOLDER_ENTITY));
    }

    #[test]
    fn non_excluded_returns_false() {
        let ex = FolderExclusions::defaults();
        assert!(!ex.is_excluded("projects"));
        assert!(!ex.is_excluded("research"));
    }
}
