//! Source kinds the engine ships, and the resolution environment every
//! engine caller of `enzyme_spec` uses so declarations expand identically.
use std::path::Path;

use anyhow::{Context, Result};

use super::EnzymePaths;

/// `.enzyme` text defining the built-in source kinds.
pub const BUILTIN_SOURCE_KINDS: &str = include_str!("builtin-sources.enzyme");

/// Resolution environment with the built-in source kinds and `{home}` set to
/// `enzyme_home`.
pub fn spec_environment(user_home: &Path, enzyme_home: &Path) -> Result<enzyme_spec::Environment> {
    let mut environment = enzyme_spec::Environment::new(user_home).with_enzyme_home(enzyme_home);
    environment
        .register_kinds(BUILTIN_SOURCE_KINDS)
        .context("built-in source kinds")?;
    Ok(environment)
}

/// [`spec_environment`] for this process: `$HOME` and `ENZYME_HOME`.
pub fn default_spec_environment() -> Result<enzyme_spec::Environment> {
    let user_home = dirs::home_dir().context("cannot resolve home directory")?;
    spec_environment(&user_home, &EnzymePaths::enzyme_home()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apple_notes_is_built_in_with_its_filter_and_default_database() {
        let environment = spec_environment(Path::new("/Users/me"), Path::new("/e")).unwrap();
        let program = enzyme_spec::parse(
            r#"workspace "notes" { source apple-notes "notes" {} learn questions from source "notes" }"#,
        )
        .unwrap();
        let resolved = enzyme_spec::resolve_in(vec![program], &environment).unwrap();
        let enzyme_spec::Source::Sqlite(notes) = &resolved.workspaces[0].sources[0] else {
            panic!("apple-notes did not expand to SQLite");
        };
        assert_eq!(
            notes.db,
            "/Users/me/Library/Group Containers/group.com.apple.notes/NoteStore.sqlite"
        );
        assert_eq!(notes.filter.as_deref(), Some("apple-notes-content-v1"));
        assert!(notes.query.contains("gzip_pb_text(d.ZDATA, '2.3.2')"));
    }
}
