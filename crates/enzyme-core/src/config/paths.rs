use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Resolved filesystem paths for an Enzyme session.
///
/// Respects `ENZYME_HOME` to override `~/.enzyme` for global state (auth, config).
/// Vault data (DB, embeddings) lives at `<vault>/.enzyme/` on normal filesystems.
/// On FUSE mounts (Cowork VMs), data is redirected to `/tmp/enzyme/` because
/// FUSE doesn't support SQLite's `xDelete` VFS operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnzymePaths {
    /// Global Enzyme home directory (`~/.enzyme` or `$ENZYME_HOME`).
    pub home: PathBuf,
    /// Path to the vault's SQLite database.
    pub vault_db: PathBuf,
}

impl EnzymePaths {
    /// Construct paths for the given vault, respecting environment overrides.
    ///
    /// On FUSE mounts, automatically redirects the DB and embeddings to
    /// `/tmp/enzyme/` so SQLite can operate on a native filesystem.
    pub fn new(vault_path: impl AsRef<Path>) -> Result<Self> {
        let enzyme_home = env::var("ENZYME_HOME").ok().filter(|s| !s.is_empty());
        Self::with_overrides(vault_path, enzyme_home.as_deref())
    }

    /// Construct paths with explicit overrides (for testing without env var mutation).
    pub fn with_overrides(
        vault_path: impl AsRef<Path>,
        enzyme_home: Option<&str>,
    ) -> Result<Self> {
        let vault_path = vault_path.as_ref();

        let home = match enzyme_home {
            Some(h) => PathBuf::from(h),
            None => {
                let home_dir =
                    dirs::home_dir().context("could not determine home directory")?;
                home_dir.join(".enzyme")
            }
        };

        let vault_db = if Self::is_fuse_mount(vault_path) {
            let local = PathBuf::from("/tmp/enzyme");
            tracing::info!(
                "FUSE mount detected — redirecting DB to {}",
                local.display()
            );
            local.join("enzyme.db")
        } else {
            vault_path.join(".enzyme").join("enzyme.db")
        };

        Ok(Self {
            home,
            vault_db,
        })
    }

    /// Resolve the Enzyme home directory.
    ///
    /// Precedence: `$ENZYME_HOME` > `~/.enzyme`.
    pub fn enzyme_home() -> Result<PathBuf> {
        if let Ok(home) = env::var("ENZYME_HOME") {
            if !home.is_empty() {
                return Ok(PathBuf::from(home));
            }
        }

        let home_dir =
            dirs::home_dir().context("could not determine home directory")?;
        Ok(home_dir.join(".enzyme"))
    }

    /// Check whether a path lives on a FUSE-style mount where SQLite WAL
    /// journaling is known to fail (e.g. Cowork VMs).
    ///
    /// Heuristic: resolve the real path and check for `/sessions/*/mnt/`.
    pub fn is_fuse_mount(path: &Path) -> bool {
        let real = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let s = real.to_string_lossy();
        // Cowork pattern: /sessions/<id>/mnt/...
        s.contains("/sessions/") && s.contains("/mnt/")
    }

    /// Path to the global config file.
    pub fn global_config(&self) -> PathBuf {
        self.home.join("config.yaml")
    }

    /// Path to the auth token file.
    pub fn auth_token(&self) -> PathBuf {
        self.home.join("auth.json")
    }

    /// Path to the embedding daemon Unix socket.
    pub fn daemon_socket(&self) -> PathBuf {
        self.home.join("embedding_daemon.sock")
    }

    /// Path to the embedding daemon PID file.
    pub fn daemon_pid(&self) -> PathBuf {
        self.home.join("embedding_daemon.pid")
    }

    /// Path to the embedding daemon log file.
    pub fn daemon_log(&self) -> PathBuf {
        self.home.join("embedding_daemon.log")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_paths() {
        let vault = PathBuf::from("/Users/alice/vault");
        let paths = EnzymePaths::with_overrides(&vault, None).unwrap();

        let home_dir = dirs::home_dir().unwrap();
        assert_eq!(paths.home, home_dir.join(".enzyme"));
        assert_eq!(paths.vault_db, vault.join(".enzyme").join("enzyme.db"));
    }

    #[test]
    fn test_enzyme_home_override() {
        let vault = PathBuf::from("/Users/alice/vault");
        let paths =
            EnzymePaths::with_overrides(&vault, Some("/tmp/custom-enzyme"))
                .unwrap();

        assert_eq!(paths.home, PathBuf::from("/tmp/custom-enzyme"));
        assert_eq!(paths.vault_db, vault.join(".enzyme").join("enzyme.db"));
    }

    #[test]
    fn test_convenience_paths() {
        let paths =
            EnzymePaths::with_overrides("/dummy", Some("/tmp/eh")).unwrap();

        assert_eq!(paths.global_config(), PathBuf::from("/tmp/eh/config.yaml"));
        assert_eq!(paths.auth_token(), PathBuf::from("/tmp/eh/auth.json"));
    }

    #[test]
    fn test_is_fuse_mount_cowork_pattern() {
        assert!(EnzymePaths::is_fuse_mount(Path::new(
            "/sessions/eloquent-zealous-planck/mnt/vault"
        )));
        assert!(EnzymePaths::is_fuse_mount(Path::new(
            "/sessions/abc-123/mnt/some/nested/vault"
        )));
    }

    #[test]
    fn test_is_fuse_mount_normal_paths() {
        assert!(!EnzymePaths::is_fuse_mount(Path::new("/Users/alice/vault")));
        assert!(!EnzymePaths::is_fuse_mount(Path::new("/home/bob/obsidian")));
        assert!(!EnzymePaths::is_fuse_mount(Path::new("/mnt/data"))); // no /sessions/
    }
}
