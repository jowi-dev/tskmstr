//! `vdiff --export-comments` seam for `tm review fix <KEY>`
//! (`src/cli/review.rs`), per `docs/plans/board-vdiff-review-loop.md`.
//!
//! `vdiff.nvim` (https://github.com/jowi-dev/vdiff) captures per-hunk review
//! comments left while reviewing a PR into `<git-dir>/vdiff/comments.json`;
//! `vdiff --export-comments`, run with its working directory set to the
//! ticket's worktree, renders that store as agent-ready markdown grouped by
//! file with `path:start-end` anchors.
//!
//! [`VdiffOps`] is the trait callers depend on; [`ShellVdiffOps`] shells out
//! to the real `vdiff` binary in production, following the same trait+fake
//! seam as [`crate::work::git::GitOps`]/[`crate::work::git::ShellGitOps`] and
//! [`crate::work::runner::ProcessSpawner`]/[`crate::work::runner::StdProcessSpawner`].
//! [`FakeVdiffOps`] is the test double, used by `cli::review`'s tests so they
//! never shell out to a real `vdiff`.

use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;

/// Errors from [`VdiffOps::export_comments`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum VdiffError {
    /// `vdiff` isn't installed, or isn't on `PATH`. Distinguished from a
    /// generic spawn failure (rather than folded into
    /// [`VdiffError::Spawn`]) so `tm review fix` can print an actionable,
    /// specific message instead of a raw "os error 2".
    #[error("`vdiff` was not found on PATH — install it from https://github.com/jowi-dev/vdiff")]
    NotFound,

    /// `vdiff` could not be spawned for a reason other than "not found"
    /// (e.g. permission denied on the binary).
    #[error("failed to spawn `vdiff --export-comments`: {message}")]
    Spawn {
        /// The underlying I/O error message.
        message: String,
    },

    /// `vdiff --export-comments` ran but exited nonzero.
    #[error("`vdiff --export-comments` failed (exit {exit_code:?}): {stderr}")]
    Command {
        /// The process exit code, if the process was not terminated by a
        /// signal.
        exit_code: Option<i32>,
        /// Captured stderr.
        stderr: String,
    },
}

/// Review-comment export for one ticket's worktree. Kept minimal — this
/// isn't a general `vdiff` client, just the one operation `tm review fix`
/// needs.
pub trait VdiffOps {
    /// Render the review comments captured for the worktree at `dir` as
    /// markdown, by running `vdiff --export-comments` with its working
    /// directory set to `dir` — `vdiff` locates `<git-dir>/vdiff/comments.json`
    /// from there itself, the same way it locates the base branch to diff
    /// against (see this module's doc comment).
    ///
    /// A store that is empty or absent is not an error: `vdiff` exits `0`
    /// and prints the literal text `"No comments."` in that case. This
    /// trait surfaces that string verbatim rather than inventing a
    /// dedicated `Ok` variant or an `Err` for it — "no comments yet" is a
    /// normal outcome of reviewing, not a fault in `vdiff` or this seam —
    /// so callers (`crate::cli::review::fix`) are the ones that compare the
    /// returned string against it.
    fn export_comments(&self, dir: &Path) -> Result<String, VdiffError>;
}

/// [`VdiffOps`] implementation that shells out to the real `vdiff` binary.
pub struct ShellVdiffOps;

impl ShellVdiffOps {
    /// Create a new shell-backed `vdiff` operations wrapper.
    pub fn new() -> Self {
        Self
    }
}

impl Default for ShellVdiffOps {
    fn default() -> Self {
        Self::new()
    }
}

impl VdiffOps for ShellVdiffOps {
    fn export_comments(&self, dir: &Path) -> Result<String, VdiffError> {
        let output = Command::new("vdiff")
            .arg("--export-comments")
            .current_dir(dir)
            .output()
            .map_err(|err| {
                if err.kind() == std::io::ErrorKind::NotFound {
                    VdiffError::NotFound
                } else {
                    VdiffError::Spawn {
                        message: err.to_string(),
                    }
                }
            })?;

        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(VdiffError::Command {
                exit_code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            })
        }
    }
}

/// Test double for [`VdiffOps`]: returns a canned result and records every
/// directory it was called with, for use by tests that don't want to shell
/// out to a real `vdiff`.
///
/// This is a plain public struct (not `#[cfg(test)]`-gated) so
/// `cli::review`'s test module can depend on it directly, matching
/// [`crate::work::git::FakeGitOps`]'s visibility.
pub struct FakeVdiffOps {
    result: Result<String, VdiffError>,
    calls: std::cell::RefCell<Vec<PathBuf>>,
}

impl FakeVdiffOps {
    /// A fake whose `export_comments` always returns `result`.
    pub fn with_export(result: Result<String, VdiffError>) -> Self {
        Self {
            result,
            calls: std::cell::RefCell::new(Vec::new()),
        }
    }

    /// Every directory `export_comments` was called with, in call order.
    pub fn calls(&self) -> Vec<PathBuf> {
        self.calls.borrow().clone()
    }
}

impl VdiffOps for FakeVdiffOps {
    fn export_comments(&self, dir: &Path) -> Result<String, VdiffError> {
        self.calls.borrow_mut().push(dir.to_path_buf());
        self.result.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_vdiff_ops_returns_the_canned_result_and_records_the_call() {
        let fake = FakeVdiffOps::with_export(Ok("## file.rs\n\ncomment".to_string()));

        let result = fake.export_comments(Path::new("/Worktrees/axiom/proj-1"));

        assert_eq!(result, Ok("## file.rs\n\ncomment".to_string()));
        assert_eq!(fake.calls(), vec![PathBuf::from("/Worktrees/axiom/proj-1")]);
    }

    #[test]
    fn fake_vdiff_ops_can_report_not_found() {
        let fake = FakeVdiffOps::with_export(Err(VdiffError::NotFound));

        let result = fake.export_comments(Path::new("/Worktrees/axiom/proj-1"));

        assert_eq!(result, Err(VdiffError::NotFound));
    }

    #[test]
    fn no_comments_is_returned_verbatim_not_specially_classified() {
        let fake = FakeVdiffOps::with_export(Ok("No comments.".to_string()));

        let result = fake.export_comments(Path::new("/Worktrees/axiom/proj-1"));

        assert_eq!(result, Ok("No comments.".to_string()));
    }
}
