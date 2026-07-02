use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

/// Resolved filesystem paths for an Enzyme session.
///
/// Respects `ENZYME_HOME` to override `~/.enzyme` for global state (auth, config).
/// Vault data (DB, embeddings) lives at `<vault>/.enzyme/` on normal filesystems.
/// On FUSE mounts (Cowork VMs), data is redirected to a session-private cache
/// because FUSE doesn't support SQLite's `xDelete` VFS operation, and concurrent
/// sessions run as different users so a shared `/tmp/enzyme/` collides on perms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnzymePaths {
    /// Global Enzyme home directory (`~/.enzyme` or `$ENZYME_HOME`).
    pub home: PathBuf,
    /// Path to the SQLite database the binary reads/writes.
    /// On FUSE this is the session-private mirror; otherwise the vault DB itself.
    pub vault_db: PathBuf,
    /// On FUSE, the durable host-side DB path inside the FUSE-mounted vault.
    /// `sync_in` copies this → `vault_db` on construction; `sync_back`
    /// copies `vault_db` → here after a successful mutating command.
    pub vault_db_origin: Option<PathBuf>,
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

    /// Construct paths from a direct database path for SDK/collection use.
    pub fn from_db_path(db_path: impl AsRef<Path>) -> Result<Self> {
        let db_path = db_path.as_ref().to_path_buf();
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory for DB: {:?}", parent))?;
        }

        let home = match env::var("ENZYME_HOME").ok().filter(|s| !s.is_empty()) {
            Some(h) => PathBuf::from(h),
            None => {
                let home_dir = dirs::home_dir().context("could not determine home directory")?;
                home_dir.join(".enzyme")
            }
        };

        Ok(Self {
            home,
            vault_db: db_path,
            vault_db_origin: None,
        })
    }

    /// Resolve a collection ID to its database path.
    ///
    /// Collections live at `~/.enzyme-sdk/collections/<id>/.enzyme/enzyme.db` by default,
    /// overridable via `ENZYME_COLLECTIONS_DIR`.
    pub fn collection_db_path(collection_id: &str) -> Result<PathBuf> {
        let base = match env::var("ENZYME_COLLECTIONS_DIR")
            .ok()
            .filter(|s| !s.is_empty())
        {
            Some(d) => PathBuf::from(d),
            None => {
                let home_dir = dirs::home_dir().context("could not determine home directory")?;
                home_dir.join(".enzyme-sdk").join("collections")
            }
        };
        Ok(base.join(collection_id).join(".enzyme").join("enzyme.db"))
    }

    /// Construct paths with explicit overrides (for testing without env var mutation).
    pub fn with_overrides(vault_path: impl AsRef<Path>, enzyme_home: Option<&str>) -> Result<Self> {
        let vault_path = vault_path.as_ref();

        let home = match enzyme_home {
            Some(h) => PathBuf::from(h),
            None => {
                let home_dir = dirs::home_dir().context("could not determine home directory")?;
                home_dir.join(".enzyme")
            }
        };

        let (vault_db, vault_db_origin) = if Self::is_fuse_mount(vault_path) {
            let origin = vault_path.join(".enzyme").join("enzyme.db");
            let mirror = Self::session_db_path(vault_path)?;
            tracing::info!(
                "FUSE mount detected — mirroring DB at {} (origin: {})",
                mirror.display(),
                origin.display()
            );
            (mirror, Some(origin))
        } else {
            (vault_path.join(".enzyme").join("enzyme.db"), None)
        };

        let paths = Self {
            home,
            vault_db,
            vault_db_origin,
        };
        paths.sync_in()?;
        Ok(paths)
    }

    /// Derive the session-private mirror DB path for a FUSE-mounted vault.
    ///
    /// Uses the per-user cache dir (resolves under `$HOME`, which Cowork
    /// scopes per-session-user) so two sessions on the same VM don't collide.
    /// Includes a hash of the canonical vault path so multiple vaults stay
    /// separate within one session.
    fn session_db_path(vault_path: &Path) -> Result<PathBuf> {
        let canonical = vault_path
            .canonicalize()
            .unwrap_or_else(|_| vault_path.to_path_buf());
        let mut hasher = Sha256::new();
        hasher.update(canonical.to_string_lossy().as_bytes());
        let hash = hasher.finalize();
        let short: String = hash[..8].iter().map(|b| format!("{:02x}", b)).collect();

        let base = dirs::cache_dir()
            .or_else(dirs::home_dir)
            .context("could not determine cache or home directory for FUSE mirror")?;
        Ok(base.join("enzyme").join(short).join("enzyme.db"))
    }

    /// Copy the durable origin DB into the session mirror if needed.
    /// No-op when there is no origin (non-FUSE paths) or when the mirror
    /// is already up-to-date.
    pub fn sync_in(&self) -> Result<()> {
        let Some(origin) = self.vault_db_origin.as_ref() else {
            return Ok(());
        };
        if let Some(parent) = self.vault_db.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create mirror dir: {:?}", parent))?;
        }
        if !origin.exists() {
            return Ok(());
        }
        let needs_copy = match (
            std::fs::metadata(origin).and_then(|m| m.modified()),
            std::fs::metadata(&self.vault_db).and_then(|m| m.modified()),
        ) {
            (Ok(o), Ok(m)) => o > m,
            (Ok(_), Err(_)) => true,
            _ => false,
        };
        if needs_copy {
            tracing::info!(
                "syncing FUSE-origin DB into session mirror ({} → {})",
                origin.display(),
                self.vault_db.display()
            );
            std::fs::copy(origin, &self.vault_db).with_context(|| {
                format!(
                    "failed to copy origin DB {} → {}",
                    origin.display(),
                    self.vault_db.display()
                )
            })?;
        }
        Ok(())
    }

    /// Return a guard that syncs the mirror DB back to the FUSE origin
    /// when dropped. Place after the `Database::open` call site so the DB
    /// connection drops first (locals drop in reverse declaration order),
    /// flushing the WAL before the file is copied.
    pub fn sync_on_drop(&self) -> SyncOnDrop<'_> {
        SyncOnDrop { paths: self }
    }

    /// Copy the session mirror DB back to the durable origin.
    /// Caller must ensure all SQLite connections are closed first.
    /// No-op when there is no origin (non-FUSE paths).
    pub fn sync_back(&self) -> Result<()> {
        let Some(origin) = self.vault_db_origin.as_ref() else {
            return Ok(());
        };
        if !self.vault_db.exists() {
            return Ok(());
        }
        if let Some(parent) = origin.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create origin dir: {:?}", parent))?;
        }
        tracing::info!(
            "syncing session mirror back to FUSE origin ({} → {})",
            self.vault_db.display(),
            origin.display()
        );
        std::fs::copy(&self.vault_db, origin).with_context(|| {
            format!(
                "failed to copy mirror DB back to origin {}",
                origin.display()
            )
        })?;
        Ok(())
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

        let home_dir = dirs::home_dir().context("could not determine home directory")?;
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
        self.home.join("config.toml")
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

    /// Path to the authentication token file.
    pub fn auth_token(&self) -> PathBuf {
        self.home.join("auth.json")
    }

    /// Path to a target-specific database for `enzyme catalyze --target`.
    ///
    /// Target data lives in `<vault>/.enzyme/targets/<hash>.db` where
    /// `<hash>` is a truncated SHA-256 of the canonical target path.
    /// This keeps all target artifacts in the source vault — the target
    /// directory is never modified.
    pub fn target_db(&self, target_path: &Path) -> PathBuf {
        let canonical = target_path
            .canonicalize()
            .unwrap_or_else(|_| target_path.to_path_buf());
        let hash = {
            let mut hasher = Sha256::new();
            hasher.update(canonical.to_string_lossy().as_bytes());
            let result = hasher.finalize();
            result[..8]
                .iter()
                .map(|b| format!("{:02x}", b))
                .collect::<String>()
        };
        self.vault_db
            .parent()
            .unwrap_or(Path::new("."))
            .join("targets")
            .join(format!("{}.db", hash))
    }
}

/// RAII guard returned by [`EnzymePaths::sync_on_drop`].
///
/// On drop, calls `sync_back` and logs (does not panic) on failure.
/// Best-effort: a partially-written origin is preferable to losing the
/// command's work entirely, and the next session will overwrite whatever's
/// there from its own mirror.
pub struct SyncOnDrop<'a> {
    paths: &'a EnzymePaths,
}

impl<'a> Drop for SyncOnDrop<'a> {
    fn drop(&mut self) {
        if let Err(e) = self.paths.sync_back() {
            tracing::warn!("FUSE DB sync_back failed: {:#}", e);
        }
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
        let paths = EnzymePaths::with_overrides(&vault, Some("/tmp/custom-enzyme")).unwrap();

        assert_eq!(paths.home, PathBuf::from("/tmp/custom-enzyme"));
        assert_eq!(paths.vault_db, vault.join(".enzyme").join("enzyme.db"));
    }

    #[test]
    fn test_convenience_paths() {
        let paths = EnzymePaths::with_overrides("/dummy", Some("/tmp/eh")).unwrap();

        assert_eq!(paths.global_config(), PathBuf::from("/tmp/eh/config.toml"));
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
    fn test_target_db_in_source_enzyme_dir() {
        let vault = PathBuf::from("/Users/alice/vault");
        let paths = EnzymePaths::with_overrides(&vault, None).unwrap();

        let target = Path::new("/Users/alice/other-vault");
        let db_path = paths.target_db(target);

        // Should be under <vault>/.enzyme/targets/<hash>.db
        assert!(db_path.starts_with(vault.join(".enzyme").join("targets")));
        assert!(db_path.extension().map_or(false, |e| e == "db"));

        // Same target should produce the same hash
        let db_path2 = paths.target_db(target);
        assert_eq!(db_path, db_path2);

        // Different target should produce a different hash
        let db_path3 = paths.target_db(Path::new("/Users/alice/yet-another"));
        assert_ne!(db_path, db_path3);
    }

    #[test]
    fn test_auth_token_path() {
        let paths = EnzymePaths::with_overrides("/dummy", Some("/tmp/eh")).unwrap();
        assert_eq!(paths.auth_token(), PathBuf::from("/tmp/eh/auth.json"));
    }

    #[test]
    fn test_is_fuse_mount_normal_paths() {
        assert!(!EnzymePaths::is_fuse_mount(Path::new("/Users/alice/vault")));
        assert!(!EnzymePaths::is_fuse_mount(Path::new("/home/bob/obsidian")));
        assert!(!EnzymePaths::is_fuse_mount(Path::new("/mnt/data"))); // no /sessions/
    }

    #[test]
    fn test_default_paths_no_origin() {
        let vault = PathBuf::from("/Users/alice/vault");
        let paths = EnzymePaths::with_overrides(&vault, None).unwrap();
        assert!(paths.vault_db_origin.is_none());
    }

    #[test]
    fn test_session_db_path_differs_per_vault() {
        let a = EnzymePaths::session_db_path(Path::new("/sessions/x/mnt/vault-one")).unwrap();
        let b = EnzymePaths::session_db_path(Path::new("/sessions/x/mnt/vault-two")).unwrap();
        assert_ne!(a, b);
        assert!(a.ends_with("enzyme.db"));
    }

    #[test]
    fn test_sync_back_noop_without_origin() {
        let vault = PathBuf::from("/Users/alice/vault");
        let paths = EnzymePaths::with_overrides(&vault, None).unwrap();
        // Non-FUSE: sync_back is a no-op even though vault_db doesn't exist.
        paths.sync_back().expect("non-FUSE sync_back must be a no-op");
        paths.sync_in().expect("non-FUSE sync_in must be a no-op");
    }
}
