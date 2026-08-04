use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use enzyme_core::config::paths::ensure_dir_symlink_aware;
use fs2::FileExt;

/// RAII guard for the `.enzyme/refresh.lock` file.
///
/// The lock is released when this struct is dropped.
pub struct RefreshLock {
    _file: File,
}

pub type ProjectionLock = RefreshLock;

fn lock_path(enzyme_dir: &Path) -> PathBuf {
    enzyme_dir.join("refresh.lock")
}

/// Try to acquire an exclusive lock on `.enzyme/refresh.lock`.
///
/// Returns `Ok(RefreshLock)` if acquired, or an error if the lock is held.
pub fn acquire_refresh_lock(enzyme_dir: &Path) -> Result<RefreshLock> {
    ensure_dir_symlink_aware(enzyme_dir).context("failed to create .enzyme directory")?;
    let path = lock_path(enzyme_dir);
    let file = File::create(&path).context("failed to create refresh.lock")?;
    file.try_lock_exclusive()
        .context("another refresh is already running")?;

    // Write PID for debugging
    let mut f = file.try_clone()?;
    write!(f, "{}", std::process::id())?;

    // Keep the inode in place. Removing a lock file while another process is
    // waiting on its open descriptor permits a third process to lock a new
    // inode concurrently.
    Ok(RefreshLock { _file: file })
}

/// Serialize the complete continuous-projection commit. This is deliberately
/// distinct from `refresh.lock`: an epoch can spend minutes doing LLM work
/// without blocking document freshness, and takes this lock only to activate.
pub fn acquire_projection_lock(enzyme_dir: &Path) -> Result<ProjectionLock> {
    ensure_dir_symlink_aware(enzyme_dir).context("failed to create .enzyme directory")?;
    let path = enzyme_dir.join("projection.lock");
    let file = File::create(&path).context("failed to create projection.lock")?;
    file.lock_exclusive()
        .context("failed to acquire continuous projection lock")?;
    let mut f = file.try_clone()?;
    write!(f, "{}", std::process::id())?;
    Ok(ProjectionLock { _file: file })
}

/// Non-blocking check: is the refresh lock currently held?
pub fn is_refresh_locked(enzyme_dir: &Path) -> bool {
    let path = lock_path(enzyme_dir);
    let Ok(file) = File::open(&path) else {
        return false;
    };
    // If we can lock it, it's NOT held — unlock immediately.
    if file.try_lock_exclusive().is_ok() {
        let _ = fs2::FileExt::unlock(&file);
        false
    } else {
        true
    }
}
