//! Shared helpers for observing catalyst-epoch background-worker completion.
//!
//! Catalyst (LLM) work no longer runs inline inside `enzyme refresh`. Both the
//! interactive path (`run_refresh_interactive`) and the fast hook path
//! (`run_refresh_fast`) do local scan+embed synchronously, then merely
//! *schedule* a catalyst epoch and — if a build is needed — spawn a detached
//! `enzyme refresh --detached --quiet` subprocess to do the actual LLM work
//! (see `crates/enzyme-cli/src/commands/init.rs` and
//! `crates/enzyme-core/src/pipeline/epoch.rs`).
//!
//! That means any test that wants to observe the *outcome* of catalyst work
//! (LLM request counts, hosted credit deduction, epoch activation/failure)
//! must poll durable state in `catalyst_epochs` rather than assume the work
//! finished by the time the parent `enzyme` process returns.

#![allow(dead_code)]

use std::path::Path;
use std::time::{Duration, Instant};

use enzyme_core::db::Database;
use libsql::Value;

/// Poll `catalyst_epochs.status` for `epoch_id` until it reaches a terminal
/// state (`active` or `failed`), returning that status.
///
/// Panics with the last observed status if `timeout` elapses first, so a
/// stuck or never-spawned background worker fails loudly instead of the
/// caller silently asserting against pre-worker state.
#[track_caller]
pub fn wait_for_epoch_terminal(db_path: &Path, epoch_id: &str, timeout: Duration) -> String {
    let start = Instant::now();
    let mut last_status: Option<String> = None;
    loop {
        let status = Database::open(db_path)
            .expect("open vault db while polling epoch status")
            .query_read(
                "SELECT status FROM catalyst_epochs WHERE epoch_id = ?1",
                vec![Value::Text(epoch_id.to_string())],
                |row| row.get::<String>(0),
            )
            .expect("query catalyst_epochs")
            .into_iter()
            .next();

        if let Some(status) = &status {
            if status == "active" || status == "failed" {
                return status.clone();
            }
        }
        last_status = status.or(last_status);

        if start.elapsed() >= timeout {
            panic!(
                "epoch {epoch_id} in {} did not reach a terminal status ('active' or \
                 'failed') within {timeout:?} (last observed status: {last_status:?}). \
                 The background worker (`enzyme refresh --detached --quiet`) may never \
                 have run, may still be running, or may be stuck.",
                db_path.display()
            );
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// Poll an arbitrary predicate until it returns `true`, or panic with `msg`
/// (plus the elapsed timeout) once `timeout` has elapsed.
#[track_caller]
pub fn wait_until(mut predicate: impl FnMut() -> bool, timeout: Duration, msg: &str) {
    let start = Instant::now();
    loop {
        if predicate() {
            return;
        }
        if start.elapsed() >= timeout {
            panic!("timed out after {timeout:?} waiting for: {msg}");
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// Extract `epoch.build_epoch_id` from a refresh JSON payload
/// (`epoch_metrics` in init.rs), panicking with the full payload if absent.
#[track_caller]
pub fn build_epoch_id(json: &serde_json::Value) -> String {
    json["epoch"]["build_epoch_id"]
        .as_str()
        .unwrap_or_else(|| {
            panic!(
                "refresh JSON has no epoch.build_epoch_id (a build should have been \
                 scheduled): {json:#}"
            )
        })
        .to_string()
}

/// Contents of `<enzyme_dir>/background_refresh.log`, for panic diagnostics.
/// The detached worker's stdout/stderr are redirected there (see
/// `spawn_background_refresh` in init.rs), so it is the only place a worker's
/// own error message (as opposed to its exit code) is visible.
pub fn background_refresh_log(db_path: &Path) -> String {
    db_path
        .parent()
        .map(|dir| dir.join("background_refresh.log"))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_else(|| "<no background_refresh.log found>".to_string())
}

/// A generous but bounded timeout for a background catalyst worker to reach
/// a terminal status. The catalyst pipeline retries a failing entity up to 3
/// times with exponential backoff (worst case ~6-7s) under a 30s per-entity
/// timeout for remote models; this leaves comfortable margin for process
/// spawn, DB snapshotting, and CI slowness.
pub const WORKER_TERMINAL_TIMEOUT: Duration = Duration::from_secs(60);
