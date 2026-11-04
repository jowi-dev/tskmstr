//! `thatch chat broadcast` seam for the board's broadcast key (GitHub issue
//! #225).
//!
//! `thatch chat broadcast <msg> --project P` delivers one message from the
//! human at the terminal to every live agent session registered under
//! project `P`. thatch labels a session's project with the `owner/name` slug
//! of its repo's `origin` remote, so [`detect_project`] derives the same
//! slug for the board's repo.
//!
//! [`ThatchRunner`] is the trait callers depend on; [`ShellThatchRunner`]
//! shells out to the real `thatch` binary, following the same trait+fake
//! seam as [`crate::work::vdiff::VdiffOps`]. [`FakeThatchRunner`] is the
//! test double.

use std::cell::RefCell;
use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;

use thiserror::Error;

/// The program name every [`ShellThatchRunner`] call spawns.
const THATCH_PROGRAM: &str = "thatch";

/// Errors from [`ThatchRunner`] calls.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ThatchError {
    /// `thatch` isn't installed, or isn't on `PATH`.
    #[error("thatch not found on PATH; broadcast not sent")]
    NotFound,

    /// `thatch` could not be spawned for a reason other than "not found".
    #[error("failed to run thatch: {message}")]
    Spawn {
        /// The underlying I/O error message.
        message: String,
    },

    /// `thatch` ran but exited nonzero. thatch prints its refusal reasons
    /// (chat disabled, message too long) on stderr.
    #[error("thatch failed: {stderr}")]
    Command {
        /// Captured stderr, trimmed.
        stderr: String,
    },
}

/// Runs `thatch` with the given arguments. Kept to one generic method so a
/// test double can record the exact command line.
pub trait ThatchRunner {
    /// Whether `thatch` can be run at all (it is on `PATH`).
    fn available(&self) -> bool;

    /// Run `thatch <args>`, returning its stdout on a zero exit.
    fn run(&self, args: &[String]) -> Result<String, ThatchError>;
}

/// [`ThatchRunner`] that spawns the real `thatch` binary.
#[derive(Debug, Default)]
pub struct ShellThatchRunner;

impl ThatchRunner for ShellThatchRunner {
    fn available(&self) -> bool {
        on_path(std::env::var_os("PATH").as_deref(), THATCH_PROGRAM)
    }

    fn run(&self, args: &[String]) -> Result<String, ThatchError> {
        let output = Command::new(THATCH_PROGRAM)
            .args(args)
            .output()
            .map_err(|err| {
                if err.kind() == std::io::ErrorKind::NotFound {
                    ThatchError::NotFound
                } else {
                    ThatchError::Spawn {
                        message: err.to_string(),
                    }
                }
            })?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(ThatchError::Command {
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            })
        }
    }
}

/// Whether `program` is a file in one of `path`'s directories.
fn on_path(path: Option<&OsStr>, program: &str) -> bool {
    path.is_some_and(|path| std::env::split_paths(path).any(|dir| dir.join(program).is_file()))
}

/// Test double for [`ThatchRunner`]: returns a canned result and records
/// every argument list it was called with.
#[derive(Debug)]
pub struct FakeThatchRunner {
    /// What [`ThatchRunner::available`] answers.
    pub available: bool,
    /// What every [`ThatchRunner::run`] call returns.
    pub result: Result<String, ThatchError>,
    /// Every argument list passed to [`ThatchRunner::run`], in call order.
    pub calls: RefCell<Vec<Vec<String>>>,
}

impl FakeThatchRunner {
    /// An available runner whose every call prints `stdout` and exits 0.
    pub fn returning(stdout: &str) -> Self {
        Self {
            available: true,
            result: Ok(stdout.to_string()),
            calls: RefCell::new(Vec::new()),
        }
    }

    /// A runner standing in for a machine without `thatch` installed.
    pub fn missing() -> Self {
        Self {
            available: false,
            result: Err(ThatchError::NotFound),
            calls: RefCell::new(Vec::new()),
        }
    }
}

impl ThatchRunner for FakeThatchRunner {
    fn available(&self) -> bool {
        self.available
    }

    fn run(&self, args: &[String]) -> Result<String, ThatchError> {
        self.calls.borrow_mut().push(args.to_vec());
        self.result.clone()
    }
}

/// The thatch project label for the repo at `dir`: the `owner/name` slug of
/// its `origin` remote, which is what thatch itself records for sessions
/// started there. `None` when the repo has no GitHub `origin` remote.
pub fn detect_project(dir: &Path) -> Option<String> {
    crate::config::detect_origin_repo(dir)
}

/// The arguments for `thatch chat broadcast` sending `message` to `project`.
/// The message goes after `--` so text starting with `--` is sent rather
/// than parsed as a flag.
pub fn broadcast_args(project: &str, message: &str) -> Vec<String> {
    vec![
        "chat".to_string(),
        "broadcast".to_string(),
        "--project".to_string(),
        project.to_string(),
        "--".to_string(),
        message.to_string(),
    ]
}

/// Who a broadcast reached, parsed from `thatch chat broadcast`'s report.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BroadcastReport {
    /// Names of the sessions the message was delivered to.
    pub delivered: Vec<String>,
    /// Names of the sessions thatch skipped, each with thatch's reason
    /// (for example `stale`).
    pub skipped: Vec<(String, String)>,
}

impl BroadcastReport {
    /// A one-line summary for the board's status line, for example
    /// `broadcast delivered to 2 sessions (a, b); skipped 1 (c: stale)`.
    pub fn summary(&self) -> String {
        let count = self.delivered.len();
        let noun = if count == 1 { "session" } else { "sessions" };
        let mut line = if count == 0 {
            "broadcast delivered to 0 sessions".to_string()
        } else {
            format!(
                "broadcast delivered to {count} {noun} ({})",
                self.delivered.join(", ")
            )
        };
        if !self.skipped.is_empty() {
            let skipped: Vec<String> = self
                .skipped
                .iter()
                .map(|(name, reason)| format!("{name}: {reason}"))
                .collect();
            line.push_str(&format!(
                "; skipped {} ({})",
                self.skipped.len(),
                skipped.join(", ")
            ));
        }
        line
    }
}

/// Parse `thatch chat broadcast`'s stdout. Each recipient is a line of the
/// form `  delivered: NAME (PROJECT) [LABEL]`, and each skipped session
/// `  skipped (REASON): NAME (PROJECT)`. Other lines (the header, the
/// out-of-scope and dedup notes) are ignored.
pub fn parse_broadcast_output(stdout: &str) -> BroadcastReport {
    let mut report = BroadcastReport::default();
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("delivered: ") {
            report.delivered.push(session_name(rest));
        } else if let Some(rest) = line.strip_prefix("skipped (")
            && let Some((reason, name)) = rest.split_once("): ")
        {
            report
                .skipped
                .push((session_name(name), reason.to_string()));
        }
    }
    report
}

/// The session name at the start of `rest`, before its ` (PROJECT)` suffix.
/// Session names never contain spaces.
fn session_name(rest: &str) -> String {
    rest.split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string()
}

/// Send `message` to every live session in `project` through `thatch`,
/// returning who received it (see [`BroadcastReport::summary`]).
pub fn broadcast(
    runner: &dyn ThatchRunner,
    project: &str,
    message: &str,
) -> Result<BroadcastReport, ThatchError> {
    let stdout = runner.run(&broadcast_args(project, message))?;
    Ok(parse_broadcast_output(&stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real `thatch chat broadcast` report (thatch 0.3.4), one line of
    /// each shape.
    const REPORT: &str = "\
Broadcast from jowi-dev to 2 of 4 registered sessions (projects: jowi-dev/tskmstr):
  delivered: npc-4-00008 (jowi-dev/tskmstr) [mcp, reads at next prompt]
  delivered: rex-protocol-00007 (jowi-dev/tskmstr) [open]
  skipped (stale): vera-antimatter-00005 (jowi-dev/tskmstr)
  out of scope (project filter): kernel-panic-00006 (other/repo)
  duplicate suppressed for 1 recipient (an identical unread broadcast stands)
";

    #[test]
    fn broadcast_runs_thatch_chat_broadcast_with_the_project() {
        let runner = FakeThatchRunner::returning(REPORT);
        broadcast(&runner, "jowi-dev/tskmstr", "main moved, rebase").unwrap();
        assert_eq!(
            runner.calls.borrow().as_slice(),
            [vec![
                "chat".to_string(),
                "broadcast".to_string(),
                "--project".to_string(),
                "jowi-dev/tskmstr".to_string(),
                "--".to_string(),
                "main moved, rebase".to_string(),
            ]]
        );
    }

    #[test]
    fn broadcast_reports_delivered_and_skipped_sessions() {
        let runner = FakeThatchRunner::returning(REPORT);
        let report = broadcast(&runner, "jowi-dev/tskmstr", "hi").unwrap();
        assert_eq!(
            report,
            BroadcastReport {
                delivered: vec!["npc-4-00008".to_string(), "rex-protocol-00007".to_string()],
                skipped: vec![("vera-antimatter-00005".to_string(), "stale".to_string())],
            }
        );
        assert_eq!(
            report.summary(),
            "broadcast delivered to 2 sessions (npc-4-00008, rex-protocol-00007); \
             skipped 1 (vera-antimatter-00005: stale)"
        );
    }

    #[test]
    fn summary_with_no_recipients_says_zero_sessions() {
        let report = parse_broadcast_output(
            "Broadcast from jowi-dev to 0 of 0 registered sessions (projects: a/b):\n",
        );
        assert_eq!(report.summary(), "broadcast delivered to 0 sessions");
    }

    #[test]
    fn summary_uses_the_singular_for_one_session() {
        let report = BroadcastReport {
            delivered: vec!["a".to_string()],
            skipped: Vec::new(),
        };
        assert_eq!(report.summary(), "broadcast delivered to 1 session (a)");
    }

    #[test]
    fn broadcast_passes_a_missing_thatch_through() {
        let runner = FakeThatchRunner::missing();
        let err = broadcast(&runner, "a/b", "hi").unwrap_err();
        assert_eq!(err, ThatchError::NotFound);
        assert_eq!(
            err.to_string(),
            "thatch not found on PATH; broadcast not sent"
        );
    }

    #[test]
    fn on_path_finds_a_program_file_in_a_path_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("thatch"), "").unwrap();
        let path = std::env::join_paths([dir.path()]).unwrap();
        assert!(on_path(Some(&path), "thatch"));
        assert!(!on_path(Some(&path), "vdiff"));
        assert!(!on_path(None, "thatch"));
    }
}
