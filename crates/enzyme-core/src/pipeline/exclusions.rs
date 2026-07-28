//! The **surface gate**: which indexed folders never become catalyst topics.
//!
//! ## The attention model this belongs to
//!
//! "What does enzyme pay attention to" is one question — *is this a candidate for
//! catalyst generation?* — enforced at two ordered gates along the pipeline
//! (`discover → index → embed → [select] → catalyze`). See
//! [`docs/design/attention-and-exclusions.md`](../../../docs/design/attention-and-exclusions.md)
//! for the full product narrative and user mental model.
//!
//! 1. **Index gate** — [`crate::document::discovery::FileDiscovery`] glob
//!    patterns. "Never read this." A folder dropped here produces no documents
//!    and therefore no entities; it is invisible downstream *for free*.
//! 2. **Surface gate** — *this module*, plus the `GENERIC_FOLDERS`/`EXCLUDED_TAGS`
//!    demotions in [`crate::pipeline::entity_map`]. Operates only over entities
//!    that survived indexing, and decides which become catalyst targets.
//!
//! The gates are ordered, not parallel: "never index" strictly implies "never
//! surface." So this set only needs to name folders that *are* indexed but must
//! not be catalyzed (`templates`, `excalidraw`, `config`, …). The dotfile/tooling
//! entries it currently also carries (`.git`, `.obsidian`, …) are belt-and-
//! suspenders — the index gate already removes them, so a folder entity for them
//! can never reach this gate. They are kept only as defense-in-depth.
//!
//! ## What [`FolderExclusions`] is
//!
//! The single, normalized set that merges enzyme's built-in **structural**
//! defaults with the user's `config.excluded_folders`. It answers exactly one
//! question: **"is this folder structural noise that should never be a catalyst
//! topic, anywhere — not even if the user explicitly curated it?"** This is the
//! *hard* tier of the surface gate. It replaced four drifted hand-maintained
//! lists (see `docs/design/entity-exclusions-refactor.md` for that history).
//!
//! It deliberately does NOT model the *soft* tier —
//! `entity_map::GENERIC_FOLDERS` (daily/notes/…) and `EXCLUDED_TAGS`. Those are
//! the "know it, but don't feature it" middle state: indexed, usable as context,
//! and surfaced only if the user explicitly names them — demoted from *automatic*
//! topic-picking, not dropped. Keeping them separate is what preserves that
//! curatable-middle-state semantic; folding them in here would make a `daily`
//! folder behave like `.git`.
//!
//! Note: the user's `config.excluded_folders` also feeds the **index gate**
//! ([`crate::document::discovery`], via `IndexingPipeline`), so `exclude` means
//! "leave it alone" (gone) — not merely "don't surface." This surface gate and
//! the index gate share that list but enforce it independently. The built-in
//! structural defaults here feed *only* the surface gate (they stay indexed).

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
