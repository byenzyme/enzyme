use std::env;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

/// Resolved filesystem paths for an Enzyme session.
///
/// Respects `ENZYME_HOME` to override `~/.enzyme` for global state (auth, config).
/// Legacy vault data lives at `<vault>/.enzyme/`; named workspace data lives at
/// `$ENZYME_HOME/workspaces/<name>/`.
/// On FUSE mounts (Cowork VMs), data is redirected to a session-private cache
/// because FUSE doesn't support SQLite's `xDelete` VFS operation, and concurrent
/// sessions run as different users so a shared `/tmp/enzyme/` collides on perms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnzymePaths {
    /// Global Enzyme home directory (`~/.enzyme` or `$ENZYME_HOME`).
    pub home: PathBuf,
    /// Global TOML configuration file. Independent from the model store so
    /// embedders can keep application configuration and weights separately.
    pub config_file: PathBuf,
    /// Directory containing local GGUF models.
    pub models_dir: PathBuf,
    /// Path to the SQLite database the binary reads/writes.
    /// On FUSE this is the session-private mirror; otherwise the vault DB itself.
    pub vault_db: PathBuf,
    /// On FUSE, the durable host-side DB path inside the FUSE-mounted vault.
    /// `sync_in` copies this → `vault_db` on construction; `sync_back`
    /// copies `vault_db` → here after a successful mutating command.
    pub vault_db_origin: Option<PathBuf>,
}

/// Create `dir` (and parents) without ever deleting or replacing a symlink.
///
/// Users keep `<vault>/.enzyme` as a symlink to a directory outside the vault
/// (e.g. to keep a large DB out of iCloud Drive sync). That layout is
/// supported: all I/O resolves through the link, and Enzyme must never remove
/// the link or the directory it points to.
///
/// `std::fs::create_dir_all` handles a symlink that resolves to an existing
/// directory, but fails with `AlreadyExists` when the path is a *dangling*
/// symlink — e.g. after an external tool (or the pre-v0.7.0 installer) removed
/// the link's target. Repair that case by recreating the missing target
/// directory through the link instead of failing or touching the link itself.
pub fn ensure_dir_symlink_aware(dir: &Path) -> std::io::Result<()> {
    let mut current = dir.to_path_buf();
    // Follow at most a short chain of symlinks; a cycle or absurd chain falls
    // through to create_dir_all's own error.
    for _ in 0..8 {
        if current.is_dir() {
            return Ok(());
        }
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = std::fs::read_link(&current)?;
                current = if target.is_absolute() {
                    target
                } else {
                    current.parent().unwrap_or(Path::new(".")).join(target)
                };
            }
            _ => break,
        }
    }
    std::fs::create_dir_all(&current)
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
            ensure_dir_symlink_aware(parent)
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
            config_file: home.join("config.toml"),
            models_dir: home.join("models"),
            home,
            vault_db: db_path,
            vault_db_origin: None,
        })
    }

    /// Resolve a named workspace to its home-rooted storage directory.
    ///
    /// A collection ID is one filesystem component, never a path. This keeps
    /// the CLI's named identity from escaping `$ENZYME_HOME/workspaces` and
    /// makes the same name safe to persist as a workspace key.
    pub fn workspace_root(name: &str) -> Result<PathBuf> {
        anyhow::ensure!(
            !name.is_empty()
                && !name.contains('/')
                && !name.contains('\\')
                && matches!(
                    Path::new(name).components().collect::<Vec<_>>().as_slice(),
                    [Component::Normal(_)]
                ),
            "workspace name must be a non-empty name, not a path: {name:?}"
        );

        Ok(Self::enzyme_home()?.join("workspaces").join(name))
    }

    /// Recover a named workspace from its home-rooted storage directory.
    pub fn bound_workspace(vault_path: &Path) -> Result<Option<String>> {
        Self::bound_workspace_with_home(vault_path, &Self::enzyme_home()?)
    }

    /// Variant used by embedded callers and tests with an explicit Enzyme home.
    pub fn bound_workspace_with_home(
        vault_path: &Path,
        enzyme_home: &Path,
    ) -> Result<Option<String>> {
        let workspaces = enzyme_home.join("workspaces");
        let Ok(relative) = vault_path.strip_prefix(&workspaces) else {
            return Ok(None);
        };
        let components: Vec<_> = relative.components().collect();
        let [Component::Normal(name)] = components.as_slice() else {
            return Ok(None);
        };
        Ok(Some(name.to_string_lossy().into_owned()))
    }

    /// Construct paths with explicit overrides (for testing without env var mutation).
    pub fn with_overrides(vault_path: impl AsRef<Path>, enzyme_home: Option<&str>) -> Result<Self> {
        Self::with_path_overrides(vault_path, enzyme_home, None, None)
    }

    /// Construct paths with independent global-state overrides.
    ///
    /// `None` retains the CLI-compatible `<enzyme_home>/config.toml` and
    /// `<enzyme_home>/models` defaults. This constructor does not consult
    /// environment variables, making it suitable for embedded/library use.
    pub fn with_path_overrides(
        vault_path: impl AsRef<Path>,
        enzyme_home: Option<&str>,
        config_file: Option<&Path>,
        models_dir: Option<&Path>,
    ) -> Result<Self> {
        let vault_path = vault_path.as_ref();

        let home = match enzyme_home {
            Some(h) => PathBuf::from(h),
            None => {
                let home_dir = dirs::home_dir().context("could not determine home directory")?;
                home_dir.join(".enzyme")
            }
        };

        let db_origin = if Self::bound_workspace_with_home(vault_path, &home)?.is_some() {
            vault_path.join("enzyme.db")
        } else {
            vault_path.join(".enzyme").join("enzyme.db")
        };
        let (vault_db, vault_db_origin) = if Self::is_fuse_mount(vault_path) {
            let origin = db_origin;
            let mirror = Self::session_db_path(vault_path)?;
            tracing::info!(
                "FUSE mount detected — mirroring DB at {} (origin: {})",
                mirror.display(),
                origin.display()
            );
            (mirror, Some(origin))
        } else {
            (db_origin, None)
        };

        let config_file = config_file
            .map(Path::to_path_buf)
            .unwrap_or_else(|| home.join("config.toml"));
        let models_dir = models_dir
            .map(Path::to_path_buf)
            .unwrap_or_else(|| home.join("models"));
        let paths = Self {
            home,
            config_file,
            models_dir,
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
        self.config_file.clone()
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
    fn workspace_name_is_a_name_not_a_path() {
        for invalid in ["", ".", "..", "nested/name", "nested\\name", "/absolute"] {
            assert!(EnzymePaths::workspace_root(invalid).is_err(), "{invalid:?}");
        }
        let root = EnzymePaths::workspace_root("imessage").unwrap();
        assert_eq!(
            root.file_name().and_then(|name| name.to_str()),
            Some("imessage")
        );
    }

    #[test]
    fn bound_workspace_uses_the_home_rooted_directory_basename() {
        let temp = tempfile::tempdir().unwrap();
        let store = temp.path().join("workspaces/imessage");

        assert_eq!(
            EnzymePaths::bound_workspace_with_home(&store, temp.path())
                .unwrap()
                .as_deref(),
            Some("imessage")
        );
        assert_eq!(
            EnzymePaths::bound_workspace_with_home(
                &temp.path().join("workspaces/imessage/nested"),
                temp.path()
            )
            .unwrap(),
            None
        );
        assert_eq!(
            EnzymePaths::bound_workspace_with_home(
                &temp.path().join("other/imessage"),
                temp.path()
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn named_workspace_database_is_directly_under_its_store() {
        let home = tempfile::tempdir().unwrap();
        let store = home.path().join("workspaces/imessage");
        let paths = EnzymePaths::with_overrides(&store, home.path().to_str()).unwrap();

        assert_eq!(paths.vault_db, store.join("enzyme.db"));
        assert_eq!(
            paths.target_db(Path::new("/target")).parent(),
            Some(store.join("targets").as_path())
        );
    }

    #[test]
    fn test_config_and_model_paths_are_independent() {
        let paths = EnzymePaths::with_path_overrides(
            "/dummy",
            Some("/tmp/eh"),
            Some(Path::new("/tmp/app/config.toml")),
            Some(Path::new("/tmp/weights")),
        )
        .unwrap();

        assert_eq!(paths.global_config(), PathBuf::from("/tmp/app/config.toml"));
        assert_eq!(paths.models_dir, PathBuf::from("/tmp/weights"));
        assert_eq!(paths.home, PathBuf::from("/tmp/eh"));
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

    #[cfg(unix)]
    #[test]
    fn test_ensure_dir_symlink_aware_preserves_link_and_repairs_target() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("outside-data");
        let link = root.path().join("vault").join(".enzyme");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        // Healthy symlinked dir: no-op, link untouched.
        ensure_dir_symlink_aware(&link).unwrap();
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert!(target.is_dir());

        // Dangling symlink (target wiped externally): the target is recreated
        // THROUGH the link; the link itself is never removed or replaced.
        std::fs::remove_dir_all(&target).unwrap();
        ensure_dir_symlink_aware(&link).unwrap();
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_link(&link).unwrap(), target);
        assert!(target.is_dir());

        // Plain directory path still works.
        let plain = root.path().join("plain/.enzyme");
        ensure_dir_symlink_aware(&plain).unwrap();
        assert!(plain.is_dir());
    }

    #[test]
    fn test_sync_back_noop_without_origin() {
        let vault = PathBuf::from("/Users/alice/vault");
        let paths = EnzymePaths::with_overrides(&vault, None).unwrap();
        // Non-FUSE: sync_back is a no-op even though vault_db doesn't exist.
        paths
            .sync_back()
            .expect("non-FUSE sync_back must be a no-op");
        paths.sync_in().expect("non-FUSE sync_in must be a no-op");
    }
}
