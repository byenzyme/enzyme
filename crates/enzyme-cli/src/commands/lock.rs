use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fs2::FileExt;

/// RAII guard for the `.enzyme/refresh.lock` file.
///
/// The lock is released when this struct is dropped.
pub struct RefreshLock {
    _file: File,
    path: PathBuf,
}

impl Drop for RefreshLock {
    fn drop(&mut self) {
        // Lock is released when File is dropped (fs2 uses flock/LockFileEx).
        // Clean up the lock file itself.
        let _ = fs::remove_file(&self.path);
    }
}

fn lock_path(enzyme_dir: &Path) -> PathBuf {
    enzyme_dir.join("refresh.lock")
}

/// Try to acquire an exclusive lock on `.enzyme/refresh.lock`.
///
/// Returns `Ok(RefreshLock)` if acquired, or an error if the lock is held.
pub fn acquire_refresh_lock(enzyme_dir: &Path) -> Result<RefreshLock> {
    fs::create_dir_all(enzyme_dir).context("failed to create .enzyme directory")?;
    let path = lock_path(enzyme_dir);
    let file = File::create(&path).context("failed to create refresh.lock")?;
    file.try_lock_exclusive()
        .context("another refresh is already running")?;

    // Write PID for debugging
    let mut f = file.try_clone()?;
    write!(f, "{}", std::process::id())?;

    Ok(RefreshLock { _file: file, path })
}

/// Non-blocking check: is the refresh lock currently held?
pub fn is_refresh_locked(enzyme_dir: &Path) -> bool {
    let path = lock_path(enzyme_dir);
    let Ok(file) = File::open(&path) else {
        return false;
    };
    // If we can lock it, it's NOT held — unlock immediately.
    if file.try_lock_exclusive().is_ok() {
        let _ = file.unlock();
        false
    } else {
        true
    }
}
