use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Resolved filesystem paths for an Enzyme session.
///
/// Respects two environment variables:
/// - `ENZYME_HOME` — overrides `~/.enzyme` for global state (auth, config, registries).
/// - `ENZYME_DB_DIR` — redirects vault databases to a writable local directory.
///   Useful when the vault itself lives on a read-only or network filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnzymePaths {
    /// Global Enzyme home directory (`~/.enzyme` or `$ENZYME_HOME`).
    pub home: PathBuf,
    /// Path to the vault's SQLite database.
    pub vault_db: PathBuf,
    /// Path to the vault's embeddings directory (HNSW index, embedding cache).
    pub embeddings_dir: PathBuf,
}

impl EnzymePaths {
    /// Construct paths for the given vault, respecting environment overrides.
    ///
    /// When `ENZYME_DB_DIR` is not set, auto-detects FUSE-mounted vault paths
    /// (e.g. Cowork VMs at `/sessions/*/mnt/`) and redirects the database to
    /// `~/.cache/enzyme/dbs` so SQLite WAL journaling works correctly.
    pub fn new(vault_path: impl AsRef<Path>) -> Result<Self> {
        let enzyme_home = env::var("ENZYME_HOME").ok().filter(|s| !s.is_empty());
        let mut enzyme_db_dir = env::var("ENZYME_DB_DIR").ok().filter(|s| !s.is_empty());

        // Auto-detect FUSE mounts where SQLite WAL will fail.
        if enzyme_db_dir.is_none() && Self::is_fuse_mount(vault_path.as_ref()) {
            let fallback = dirs::home_dir()
                .map(|h| h.join(".cache").join("enzyme").join("dbs"));
            if let Some(fb) = fallback {
                tracing::info!(
                    "vault appears to be on a FUSE mount — redirecting DB to {}",
                    fb.display()
                );
                enzyme_db_dir = Some(fb.to_string_lossy().into_owned());
            }
        }

        Self::with_overrides(vault_path, enzyme_home.as_deref(), enzyme_db_dir.as_deref())
    }

    /// Construct paths with explicit overrides (for testing without env var mutation).
    pub fn with_overrides(
        vault_path: impl AsRef<Path>,
        enzyme_home: Option<&str>,
        enzyme_db_dir: Option<&str>,
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

        let (vault_db, embeddings_dir) = match enzyme_db_dir {
            Some(db_dir) => {
                let db_base = Self::vault_db_base(vault_path, db_dir);
                (
                    db_base.join("enzyme.db"),
                    db_base.join("enzyme-embeddings"),
                )
            }
            None => (
                vault_path.join(".enzyme").join("enzyme.db"),
                vault_path.join(".enzyme-embeddings"),
            ),
        };

        Ok(Self {
            home,
            vault_db,
            embeddings_dir,
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
    fn is_fuse_mount(path: &Path) -> bool {
        let real = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let s = real.to_string_lossy();
        // Cowork pattern: /sessions/<id>/mnt/...
        s.contains("/sessions/") && s.contains("/mnt/")
    }

    /// Compute a stable subdirectory under `db_dir` for a given vault path.
    ///
    /// Uses a simple hash of the vault's canonical path to avoid collisions
    /// while keeping directory names filesystem-friendly.
    fn vault_db_base(vault_path: &Path, db_dir: &str) -> PathBuf {
        let hash = Self::path_hash(vault_path);
        PathBuf::from(db_dir).join(hash)
    }

    /// Produce a short, deterministic, filesystem-safe hash of a path.
    ///
    /// We use a simple FNV-1a-style hash (64-bit) rendered as hex.
    /// This is NOT cryptographic — it only needs to be collision-resistant
    /// enough for a handful of vaults.
    fn path_hash(path: &Path) -> String {
        let bytes = path.to_string_lossy().as_bytes().to_vec();
        let mut hash: u64 = 0xcbf29ce484222325; // FNV offset basis
        for &b in &bytes {
            hash ^= b as u64;
            hash = hash.wrapping_mul(0x100000001b3); // FNV prime
        }
        format!("{:016x}", hash)
    }

    /// Path to the global config file.
    pub fn global_config(&self) -> PathBuf {
        self.home.join("config.yaml")
    }

    /// Path to the global vault registry.
    pub fn vault_registry(&self) -> PathBuf {
        self.home.join("vaults.json")
    }

    /// Path to the global source registry.
    pub fn source_registry(&self) -> PathBuf {
        self.home.join("sources.json")
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
        let paths = EnzymePaths::with_overrides(&vault, None, None).unwrap();

        let home_dir = dirs::home_dir().unwrap();
        assert_eq!(paths.home, home_dir.join(".enzyme"));
        assert_eq!(paths.vault_db, vault.join(".enzyme").join("enzyme.db"));
        assert_eq!(paths.embeddings_dir, vault.join(".enzyme-embeddings"));
    }

    #[test]
    fn test_enzyme_home_override() {
        let vault = PathBuf::from("/Users/alice/vault");
        let paths =
            EnzymePaths::with_overrides(&vault, Some("/tmp/custom-enzyme"), None)
                .unwrap();

        assert_eq!(paths.home, PathBuf::from("/tmp/custom-enzyme"));
        assert_eq!(paths.vault_db, vault.join(".enzyme").join("enzyme.db"));
    }

    #[test]
    fn test_enzyme_db_dir_override() {
        let vault = PathBuf::from("/Users/alice/vault");
        let paths =
            EnzymePaths::with_overrides(&vault, None, Some("/tmp/enzyme-dbs"))
                .unwrap();

        assert!(paths.vault_db.starts_with("/tmp/enzyme-dbs"));
        assert!(paths.vault_db.ends_with("enzyme.db"));
        assert!(paths.embeddings_dir.starts_with("/tmp/enzyme-dbs"));
        assert!(paths.embeddings_dir.ends_with("enzyme-embeddings"));
    }

    #[test]
    fn test_both_overrides() {
        let vault = PathBuf::from("/mnt/vault");
        let paths = EnzymePaths::with_overrides(
            &vault,
            Some("/opt/enzyme-home"),
            Some("/opt/enzyme-dbs"),
        )
        .unwrap();

        assert_eq!(paths.home, PathBuf::from("/opt/enzyme-home"));
        assert!(paths.vault_db.starts_with("/opt/enzyme-dbs"));
        assert!(paths.embeddings_dir.starts_with("/opt/enzyme-dbs"));
    }

    #[test]
    fn test_path_hash_deterministic() {
        let hash1 = EnzymePaths::path_hash(Path::new("/Users/alice/vault"));
        let hash2 = EnzymePaths::path_hash(Path::new("/Users/alice/vault"));
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_path_hash_different_paths_differ() {
        let hash1 = EnzymePaths::path_hash(Path::new("/Users/alice/vault"));
        let hash2 = EnzymePaths::path_hash(Path::new("/Users/bob/vault"));
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_path_hash_is_hex_string() {
        let hash = EnzymePaths::path_hash(Path::new("/some/path"));
        assert_eq!(hash.len(), 16);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_convenience_paths() {
        let paths =
            EnzymePaths::with_overrides("/dummy", Some("/tmp/eh"), None).unwrap();

        assert_eq!(paths.global_config(), PathBuf::from("/tmp/eh/config.yaml"));
        assert_eq!(paths.vault_registry(), PathBuf::from("/tmp/eh/vaults.json"));
        assert_eq!(
            paths.source_registry(),
            PathBuf::from("/tmp/eh/sources.json")
        );
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
