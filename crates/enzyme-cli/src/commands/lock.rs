use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use enzyme_core::config::paths::ensure_dir_symlink_aware;
use fs2::FileExt;

use super::exit::CliFailure;

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
    let file = open_lock_file(&path)?;
    file.try_lock_exclusive()
        .context("another refresh is already running")?;
    record_holder(&file)?;

    // Keep the inode in place. Removing a lock file while another process is
    // waiting on its open descriptor permits a third process to lock a new
    // inode concurrently.
    Ok(RefreshLock { _file: file })
}

/// Default for `--lock-timeout`: how long a foreground init/refresh waits for
/// another holder of the workspace before exiting busy.
pub const DEFAULT_LOCK_TIMEOUT_SECS: u64 = 1800;

/// How a foreground command announces that it is waiting for a lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitNotice {
    Silent,
    Text,
    JsonProgress,
}

static WAIT_POLICY: OnceLock<(Duration, WaitNotice)> = OnceLock::new();

/// Configure lock waiting for this process (from `--lock-timeout`, `--quiet`,
/// `--json-progress`). Later calls are ignored.
pub fn configure_lock_wait(timeout: Duration, notice: WaitNotice) {
    let _ = WAIT_POLICY.set((timeout, notice));
}

fn wait_policy() -> (Duration, WaitNotice) {
    *WAIT_POLICY.get_or_init(|| {
        (
            Duration::from_secs(DEFAULT_LOCK_TIMEOUT_SECS),
            WaitNotice::Text,
        )
    })
}

/// Serialize foreground `init`/`refresh` on one workspace (`workspace.lock`
/// beside its index). A concurrent caller waits up to the configured timeout,
/// announcing the wait, then fails with the busy exit code.
///
/// This is separate from `refresh.lock`: a detached catalyst worker may hold
/// that for minutes while foreground refreshes keep documents fresh.
pub fn acquire_workspace_lock(enzyme_dir: &Path, workspace: &str) -> Result<RefreshLock> {
    let path = enzyme_dir.join("workspace.lock");
    wait_exclusive(
        enzyme_dir,
        &path,
        &format!("another enzyme init/refresh of {workspace}"),
    )
}

/// Wait for any detached init/catalyst worker on this workspace to finish and
/// hold `refresh.lock` meanwhile, so a foreground rebuild never resets or
/// rewrites catalysts underneath one.
pub fn wait_for_refresh_lock(enzyme_dir: &Path) -> Result<RefreshLock> {
    wait_exclusive(
        enzyme_dir,
        &lock_path(enzyme_dir),
        "a background catalyst worker",
    )
}

fn wait_exclusive(enzyme_dir: &Path, path: &Path, holder: &str) -> Result<RefreshLock> {
    ensure_dir_symlink_aware(enzyme_dir).context("failed to create .enzyme directory")?;
    let file = open_lock_file(path)?;
    let (timeout, notice) = wait_policy();
    let start = Instant::now();
    let mut announced = false;
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => break,
            Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {}
            Err(error) => {
                return Err(error).with_context(|| format!("failed to lock {}", path.display()));
            }
        }
        let pid = holder_pid(path);
        if start.elapsed() >= timeout {
            return Err(CliFailure::busy(anyhow::anyhow!(
                "workspace busy: {holder}{} still holds {} after waiting {}s (see --lock-timeout)",
                pid.map(|pid| format!(" (pid {pid})")).unwrap_or_default(),
                path.display(),
                timeout.as_secs()
            )));
        }
        if !announced {
            announced = true;
            match notice {
                WaitNotice::Silent => {}
                WaitNotice::Text => eprintln!(
                    "Waiting for {holder}{} to finish (up to {}s)…",
                    pid.map(|pid| format!(" (pid {pid})")).unwrap_or_default(),
                    timeout.as_secs()
                ),
                WaitNotice::JsonProgress => eprintln!(
                    "{}",
                    serde_json::json!({
                        "stage": "lock",
                        "status": "waiting",
                        "lock": path.file_name().map(|n| n.to_string_lossy().into_owned()),
                        "holder_pid": pid,
                        "timeout_secs": timeout.as_secs(),
                    })
                ),
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    record_holder(&file)?;
    Ok(RefreshLock { _file: file })
}

/// Open without truncating: a waiter must not erase the holder's PID.
fn open_lock_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))
}

/// Record the holder's PID for diagnostics and waiting messages.
fn record_holder(file: &File) -> Result<()> {
    file.set_len(0)?;
    let mut f = file.try_clone()?;
    std::io::Seek::rewind(&mut f)?;
    write!(f, "{}", std::process::id())?;
    Ok(())
}

fn holder_pid(path: &Path) -> Option<u32> {
    let mut text = String::new();
    File::open(path).ok()?.read_to_string(&mut text).ok()?;
    text.trim().parse().ok()
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
