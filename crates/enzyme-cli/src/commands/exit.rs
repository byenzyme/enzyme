//! Process exit codes. Documented in `docs/cli-exit-codes.md`; keep the two
//! in step.
//!
//! | Code | Meaning |
//! |------|---------|
//! | 0 | Success (including a run that completed with warnings) |
//! | 1 | Index or runtime failure (any error not classified below) |
//! | 2 | Usage error: bad flags or arguments (clap), invalid workspace name |
//! | 3 | Configuration or program error: unreadable/invalid config, undeclared workspace, missing source |
//! | 4 | Workspace busy: another init/refresh held the workspace lock past `--lock-timeout` |
//! | 5 | Catalysts failed: `init` built the index, but catalyst generation failed its quality gate |

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitKind {
    Runtime = 1,
    Usage = 2,
    Config = 3,
    Busy = 4,
    CatalystsFailed = 5,
}

/// An error that selects a specific exit code. Wrap the underlying error with
/// [`CliFailure::config`] etc.; `main` finds it anywhere in the chain.
#[derive(Debug)]
pub struct CliFailure {
    pub kind: ExitKind,
    error: anyhow::Error,
}

impl CliFailure {
    pub fn usage(error: impl Into<anyhow::Error>) -> anyhow::Error {
        Self::wrap(ExitKind::Usage, error)
    }

    pub fn config(error: impl Into<anyhow::Error>) -> anyhow::Error {
        Self::wrap(ExitKind::Config, error)
    }

    pub fn busy(error: impl Into<anyhow::Error>) -> anyhow::Error {
        Self::wrap(ExitKind::Busy, error)
    }

    fn wrap(kind: ExitKind, error: impl Into<anyhow::Error>) -> anyhow::Error {
        anyhow::Error::new(Self {
            kind,
            error: error.into(),
        })
    }
}

impl fmt::Display for CliFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#}", self.error)
    }
}

impl std::error::Error for CliFailure {}

/// A classified failure that still produced a machine-readable summary (for
/// example `init --quiet` whose catalysts failed after the index was built).
/// `main` prints `summary` on stdout for quiet/JSON callers before exiting.
#[derive(Debug)]
pub struct FailureWithSummary {
    pub kind: ExitKind,
    pub summary: serde_json::Value,
    message: String,
}

impl FailureWithSummary {
    pub fn new(kind: ExitKind, summary: serde_json::Value, message: String) -> anyhow::Error {
        anyhow::Error::new(Self {
            kind,
            summary,
            message,
        })
    }
}

impl fmt::Display for FailureWithSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for FailureWithSummary {}

/// The summary carried by a [`FailureWithSummary`] anywhere in the chain.
pub fn failure_summary(error: &anyhow::Error) -> Option<&serde_json::Value> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<FailureWithSummary>())
        .map(|failure| &failure.summary)
}

/// The exit code for an error returned from `main`.
pub fn exit_kind(error: &anyhow::Error) -> ExitKind {
    error
        .chain()
        .find_map(|cause| {
            cause
                .downcast_ref::<CliFailure>()
                .map(|failure| failure.kind)
                .or_else(|| {
                    cause
                        .downcast_ref::<FailureWithSummary>()
                        .map(|failure| failure.kind)
                })
        })
        .unwrap_or(ExitKind::Runtime)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    #[test]
    fn classified_errors_survive_added_context() {
        let error = Err::<(), _>(CliFailure::busy(anyhow::anyhow!("held")))
            .context("init failed")
            .unwrap_err();
        assert_eq!(exit_kind(&error), ExitKind::Busy);
        assert_eq!(exit_kind(&anyhow::anyhow!("boom")), ExitKind::Runtime);
        assert_eq!(
            CliFailure::config(anyhow::anyhow!("inner").context("outer")).to_string(),
            "outer: inner"
        );
    }
}
