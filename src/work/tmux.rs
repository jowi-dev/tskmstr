//! tmux session/window operations for the lane runner, ported from
//! devtools' `~/devtools/work.ml`.
//!
//! [`TmuxOps`] is the trait callers depend on; [`ShellTmuxOps`] is the
//! `tmux`-shelling-out implementation used in production. [`FakeTmuxOps`]
//! is a test double for use by tests that don't want to shell out.
//!
//! tm owns session create/attach/list/kill (see
//! `docs/plans/runner-port.md` §1, "The tmux question"); *what's in* a
//! session — the extra window names beyond the primary one — is a config
//! knob ([`crate::config::WorkConfig::tmux_windows`] /
//! [`crate::config::WorkConfig::tmux_primary_window`]), not hardcoded here.
//! [`window_creation_sequence`] is the pure function that turns that config
//! into the ordered list of window names a new session should be built
//! with.
//!
//! Every `tmux` argv below mirrors `work.ml`'s exactly:
//!
//! ```ocaml
//! let tmux_has_session name =
//!   let cmd = sprintf "tmux has-session -t '%s' 2>/dev/null" name in
//!   Sys.command cmd = 0
//!
//! let tmux_new_session name dir =
//!   let cmd = sprintf "tmux new-session -d -s '%s' -c '%s' -n code" name dir in
//!   ...
//!   let cmd = sprintf "tmux new-window -t '%s' -n '%s' -c '%s'" name win_name dir in
//!   ...
//!   let cmd = sprintf "tmux select-window -t '%s:code'" name in
//!   ...
//!
//! let tmux_attach name =
//!   let cmd = sprintf "tmux attach-session -t '%s'" name in
//!   ...
//! ```
//!
//! `work.ml` has no `$TMUX`/inside-vs-outside-tmux branch anywhere: it
//! always shells out to `tmux attach-session -t '<name>'` regardless of
//! whether the caller is already inside a tmux client. [`ShellTmuxOps`]
//! ports that verbatim (no `switch-client` path) rather than inventing a
//! distinction the OCaml version never made.
//!
//! `tmux kill-session -t '<name>'` and the `list-sessions` format string
//! (`'#{session_name}|#{session_path}'`, parsed by splitting on `|` and
//! discarding malformed lines) are ported the same way.

use std::process::Command;

use thiserror::Error;

/// Errors that can occur while shelling out to `tmux`.
#[derive(Debug, Clone, Error)]
pub enum TmuxError {
    /// The `tmux` binary could not be spawned.
    #[error("failed to run `{command}`: {message}")]
    Spawn {
        /// The command that could not be spawned, e.g. `tmux new-session`.
        command: String,
        /// The underlying spawn error message.
        message: String,
    },

    /// The command ran but exited with a failure.
    #[error("`{command}` failed (exit {exit_code:?}): {stderr}")]
    Command {
        /// The command that failed, e.g. `tmux new-session`.
        command: String,
        /// The process exit code, if the process was not terminated by a signal.
        exit_code: Option<i32>,
        /// Captured stderr.
        stderr: String,
    },
}

/// One row of `tmux list-sessions` output: a session's name and the working
/// directory its first window was created in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxSession {
    /// `#{session_name}`.
    pub name: String,
    /// `#{session_path}`.
    pub path: String,
}

/// Behavior tskmstr needs from `tmux` to provision and manage lane/worktree
/// sessions.
pub trait TmuxOps {
    /// Whether a session named `name` currently exists
    /// (`tmux has-session -t <name>`).
    ///
    /// Never errors on "no such session" (`tmux`'s normal, expected exit
    /// code 1 for a missing session) — that case is `Ok(false)`. Only a
    /// failure to run `tmux` at all is an error.
    fn has_session(&self, name: &str) -> Result<bool, TmuxError>;

    /// Create a new detached session named `name`, rooted at `dir`, with
    /// its first window named `primary_window`
    /// (`tmux new-session -d -s <name> -c <dir> -n <primary_window>`).
    fn new_session(&self, name: &str, dir: &str, primary_window: &str) -> Result<(), TmuxError>;

    /// Create an additional window named `window_name` in session `name`,
    /// rooted at `dir` (`tmux new-window -t <name> -n <window_name> -c
    /// <dir>`).
    fn new_window(&self, name: &str, window_name: &str, dir: &str) -> Result<(), TmuxError>;

    /// Select window `window` in session `name`
    /// (`tmux select-window -t <name>:<window>`).
    fn select_window(&self, name: &str, window: &str) -> Result<(), TmuxError>;

    /// Attach the current terminal to session `name`
    /// (`tmux attach-session -t <name>`). See the module docs for why this
    /// never branches on being already inside tmux.
    fn attach(&self, name: &str) -> Result<(), TmuxError>;

    /// Kill session `name` (`tmux kill-session -t <name>`).
    fn kill_session(&self, name: &str) -> Result<(), TmuxError>;

    /// List all running tmux sessions
    /// (`tmux list-sessions -F '#{session_name}|#{session_path}'`).
    ///
    /// Mirrors `work.ml`'s tolerance: when no `tmux` server is running (or
    /// there are no sessions), this returns `Ok(vec![])` rather than an
    /// error, matching `Unix.open_process_in`'s behavior of never
    /// inspecting the child's exit status.
    fn list_sessions(&self) -> Result<Vec<TmuxSession>, TmuxError>;
}

/// Given a lane/session's configured extra windows and primary window name,
/// return the ordered sequence of window names a new session should be
/// built with: the primary window first, then each extra window in
/// configured order.
///
/// Defaults applied when unset, per `docs/plans/runner-port.md` §2: the
/// primary window defaults to `"code"` (the name `work.ml` hardcoded into
/// `new-session -n code`), and the extra-window list defaults to empty — a
/// minimal default, not `work.ml`'s personal `["fish"; "claude"; "server"]`
/// set, which is now purely a config value (see the module docs).
pub fn window_creation_sequence(
    tmux_windows: &[String],
    tmux_primary_window: Option<&str>,
) -> Vec<String> {
    let primary = tmux_primary_window.unwrap_or("code").to_string();
    let mut sequence = Vec::with_capacity(1 + tmux_windows.len());
    sequence.push(primary);
    sequence.extend(tmux_windows.iter().cloned());
    sequence
}

fn has_session_args(name: &str) -> Vec<String> {
    vec![
        "has-session".to_string(),
        "-t".to_string(),
        name.to_string(),
    ]
}

fn new_session_args(name: &str, dir: &str, primary_window: &str) -> Vec<String> {
    vec![
        "new-session".to_string(),
        "-d".to_string(),
        "-s".to_string(),
        name.to_string(),
        "-c".to_string(),
        dir.to_string(),
        "-n".to_string(),
        primary_window.to_string(),
    ]
}

fn new_window_args(name: &str, window_name: &str, dir: &str) -> Vec<String> {
    vec![
        "new-window".to_string(),
        "-t".to_string(),
        name.to_string(),
        "-n".to_string(),
        window_name.to_string(),
        "-c".to_string(),
        dir.to_string(),
    ]
}

fn select_window_args(name: &str, window: &str) -> Vec<String> {
    vec![
        "select-window".to_string(),
        "-t".to_string(),
        format!("{name}:{window}"),
    ]
}

fn attach_args(name: &str) -> Vec<String> {
    vec![
        "attach-session".to_string(),
        "-t".to_string(),
        name.to_string(),
    ]
}

fn kill_session_args(name: &str) -> Vec<String> {
    vec![
        "kill-session".to_string(),
        "-t".to_string(),
        name.to_string(),
    ]
}

fn list_sessions_args() -> Vec<String> {
    vec![
        "list-sessions".to_string(),
        "-F".to_string(),
        "#{session_name}|#{session_path}".to_string(),
    ]
}

/// Parse `tmux list-sessions -F '#{session_name}|#{session_path}'` output,
/// mirroring `work.ml`'s `worktree_list`: split each line on `|`, keep only
/// lines with exactly two fields, and silently drop anything else.
fn parse_list_sessions_output(stdout: &str) -> Vec<TmuxSession> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('|');
            let name = parts.next()?;
            let path = parts.next()?;
            if parts.next().is_some() {
                return None;
            }
            Some(TmuxSession {
                name: name.to_string(),
                path: path.to_string(),
            })
        })
        .collect()
}

/// [`TmuxOps`] implementation that shells out to a real `tmux` binary. Used
/// in production.
#[derive(Debug, Default, Clone, Copy)]
pub struct ShellTmuxOps;

impl ShellTmuxOps {
    /// Create a new [`ShellTmuxOps`].
    pub fn new() -> Self {
        Self
    }
}

fn run(command_label: &str, args: &[String]) -> Result<std::process::Output, TmuxError> {
    Command::new("tmux")
        .args(args)
        .output()
        .map_err(|err| TmuxError::Spawn {
            command: command_label.to_string(),
            message: err.to_string(),
        })
}

fn require_success(command_label: &str, output: &std::process::Output) -> Result<(), TmuxError> {
    if output.status.success() {
        Ok(())
    } else {
        Err(TmuxError::Command {
            command: command_label.to_string(),
            exit_code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        })
    }
}

impl TmuxOps for ShellTmuxOps {
    fn has_session(&self, name: &str) -> Result<bool, TmuxError> {
        // `tmux has-session` exits 0 when the session exists and 1
        // (unremarkably) when it does not — that's not a failure to report,
        // matching `work.ml`'s `Sys.command cmd = 0`.
        let output = run("tmux has-session", &has_session_args(name))?;
        Ok(output.status.success())
    }

    fn new_session(&self, name: &str, dir: &str, primary_window: &str) -> Result<(), TmuxError> {
        let output = run(
            "tmux new-session",
            &new_session_args(name, dir, primary_window),
        )?;
        require_success("tmux new-session", &output)
    }

    fn new_window(&self, name: &str, window_name: &str, dir: &str) -> Result<(), TmuxError> {
        let output = run("tmux new-window", &new_window_args(name, window_name, dir))?;
        require_success("tmux new-window", &output)
    }

    fn select_window(&self, name: &str, window: &str) -> Result<(), TmuxError> {
        let output = run("tmux select-window", &select_window_args(name, window))?;
        require_success("tmux select-window", &output)
    }

    fn attach(&self, name: &str) -> Result<(), TmuxError> {
        // Interactive: inherit the caller's stdio (the default for
        // `status()`, unlike `output()`) so the terminal actually attaches,
        // matching `Sys.command`'s inherited-stdio semantics.
        let status = Command::new("tmux")
            .args(attach_args(name))
            .status()
            .map_err(|err| TmuxError::Spawn {
                command: "tmux attach-session".to_string(),
                message: err.to_string(),
            })?;
        if status.success() {
            Ok(())
        } else {
            Err(TmuxError::Command {
                command: "tmux attach-session".to_string(),
                exit_code: status.code(),
                stderr: String::new(),
            })
        }
    }

    fn kill_session(&self, name: &str) -> Result<(), TmuxError> {
        let output = run("tmux kill-session", &kill_session_args(name))?;
        require_success("tmux kill-session", &output)
    }

    fn list_sessions(&self) -> Result<Vec<TmuxSession>, TmuxError> {
        // `work.ml` reads via `Unix.open_process_in` and never inspects the
        // child's exit status (and redirects stderr to /dev/null), so a
        // no-server-running / no-sessions case is tolerated as empty output
        // rather than an error.
        let output = run("tmux list-sessions", &list_sessions_args())?;
        Ok(parse_list_sessions_output(&String::from_utf8_lossy(
            &output.stdout,
        )))
    }
}

/// A [`TmuxOps`] test double: returns canned results and records every call
/// made against it, for use by tests that don't want to shell out to a real
/// `tmux`.
///
/// This is a plain public struct (not `#[cfg(test)]`-gated) so other test
/// code in the crate can depend on it directly, matching
/// [`crate::github::gh_cli::FakeGhCli`]'s pattern.
#[derive(Debug)]
pub struct FakeTmuxOps {
    has_session_result: std::cell::RefCell<Result<bool, TmuxError>>,
    list_sessions_result: std::cell::RefCell<Result<Vec<TmuxSession>, TmuxError>>,
    calls: std::cell::RefCell<Vec<TmuxCall>>,
}

impl Default for FakeTmuxOps {
    fn default() -> Self {
        Self::new()
    }
}

/// One recorded invocation against a [`FakeTmuxOps`], in call order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TmuxCall {
    /// `has_session(name)`.
    HasSession(String),
    /// `new_session(name, dir, primary_window)`.
    NewSession {
        /// Session name.
        name: String,
        /// Session working directory.
        dir: String,
        /// Primary window name.
        primary_window: String,
    },
    /// `new_window(name, window_name, dir)`.
    NewWindow {
        /// Session name.
        name: String,
        /// Window name.
        window_name: String,
        /// Window working directory.
        dir: String,
    },
    /// `select_window(name, window)`.
    SelectWindow {
        /// Session name.
        name: String,
        /// Window name.
        window: String,
    },
    /// `attach(name)`.
    Attach(String),
    /// `kill_session(name)`.
    KillSession(String),
    /// `list_sessions()`.
    ListSessions,
}

impl FakeTmuxOps {
    /// Create a fake where `has_session` returns `Ok(false)` and
    /// `list_sessions` returns `Ok(vec![])` unless overridden.
    pub fn new() -> Self {
        Self {
            has_session_result: std::cell::RefCell::new(Ok(false)),
            list_sessions_result: std::cell::RefCell::new(Ok(Vec::new())),
            calls: std::cell::RefCell::new(Vec::new()),
        }
    }

    /// Set the result `has_session` will return.
    pub fn with_has_session(self, result: Result<bool, TmuxError>) -> Self {
        *self.has_session_result.borrow_mut() = result;
        self
    }

    /// Set the result `list_sessions` will return.
    pub fn with_list_sessions(self, result: Result<Vec<TmuxSession>, TmuxError>) -> Self {
        *self.list_sessions_result.borrow_mut() = result;
        self
    }

    /// The calls made against this fake, in call order.
    pub fn calls(&self) -> Vec<TmuxCall> {
        self.calls.borrow().clone()
    }
}

impl TmuxOps for FakeTmuxOps {
    fn has_session(&self, name: &str) -> Result<bool, TmuxError> {
        self.calls
            .borrow_mut()
            .push(TmuxCall::HasSession(name.to_string()));
        self.has_session_result.borrow().clone()
    }

    fn new_session(&self, name: &str, dir: &str, primary_window: &str) -> Result<(), TmuxError> {
        self.calls.borrow_mut().push(TmuxCall::NewSession {
            name: name.to_string(),
            dir: dir.to_string(),
            primary_window: primary_window.to_string(),
        });
        Ok(())
    }

    fn new_window(&self, name: &str, window_name: &str, dir: &str) -> Result<(), TmuxError> {
        self.calls.borrow_mut().push(TmuxCall::NewWindow {
            name: name.to_string(),
            window_name: window_name.to_string(),
            dir: dir.to_string(),
        });
        Ok(())
    }

    fn select_window(&self, name: &str, window: &str) -> Result<(), TmuxError> {
        self.calls.borrow_mut().push(TmuxCall::SelectWindow {
            name: name.to_string(),
            window: window.to_string(),
        });
        Ok(())
    }

    fn attach(&self, name: &str) -> Result<(), TmuxError> {
        self.calls
            .borrow_mut()
            .push(TmuxCall::Attach(name.to_string()));
        Ok(())
    }

    fn kill_session(&self, name: &str) -> Result<(), TmuxError> {
        self.calls
            .borrow_mut()
            .push(TmuxCall::KillSession(name.to_string()));
        Ok(())
    }

    fn list_sessions(&self) -> Result<Vec<TmuxSession>, TmuxError> {
        self.calls.borrow_mut().push(TmuxCall::ListSessions);
        self.list_sessions_result.borrow().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- argv construction ---

    #[test]
    fn has_session_args_match_work_ml() {
        assert_eq!(
            has_session_args("axiom-lane"),
            vec!["has-session", "-t", "axiom-lane"]
        );
    }

    #[test]
    fn new_session_args_match_work_ml() {
        assert_eq!(
            new_session_args("axiom-lane", "/repo/lane", "code"),
            vec![
                "new-session",
                "-d",
                "-s",
                "axiom-lane",
                "-c",
                "/repo/lane",
                "-n",
                "code"
            ]
        );
    }

    #[test]
    fn new_window_args_match_work_ml() {
        assert_eq!(
            new_window_args("axiom-lane", "fish", "/repo/lane"),
            vec![
                "new-window",
                "-t",
                "axiom-lane",
                "-n",
                "fish",
                "-c",
                "/repo/lane"
            ]
        );
    }

    #[test]
    fn select_window_args_match_work_ml() {
        assert_eq!(
            select_window_args("axiom-lane", "code"),
            vec!["select-window", "-t", "axiom-lane:code"]
        );
    }

    #[test]
    fn attach_args_match_work_ml() {
        assert_eq!(
            attach_args("axiom-lane"),
            vec!["attach-session", "-t", "axiom-lane"]
        );
    }

    #[test]
    fn kill_session_args_match_work_ml() {
        assert_eq!(
            kill_session_args("axiom-lane"),
            vec!["kill-session", "-t", "axiom-lane"]
        );
    }

    #[test]
    fn list_sessions_args_match_work_ml() {
        assert_eq!(
            list_sessions_args(),
            vec!["list-sessions", "-F", "#{session_name}|#{session_path}"]
        );
    }

    // --- list-sessions output parsing ---

    #[test]
    fn parses_multiple_session_lines() {
        let stdout =
            "axiom-lane|/Users/jowi/Worktrees/axiom/axiom-lane\nmain|/Users/jowi/Projects/axiom\n";
        assert_eq!(
            parse_list_sessions_output(stdout),
            vec![
                TmuxSession {
                    name: "axiom-lane".to_string(),
                    path: "/Users/jowi/Worktrees/axiom/axiom-lane".to_string(),
                },
                TmuxSession {
                    name: "main".to_string(),
                    path: "/Users/jowi/Projects/axiom".to_string(),
                },
            ]
        );
    }

    #[test]
    fn empty_output_yields_no_sessions() {
        assert_eq!(parse_list_sessions_output(""), Vec::new());
    }

    #[test]
    fn malformed_lines_are_dropped_not_erroring() {
        // No `tmux` server running produces no output at all in practice,
        // but a stray line missing the delimiter (or with too many) should
        // be silently skipped, mirroring `work.ml`'s `| _ -> ()` catch-all.
        let stdout = "no-delimiter-here\nlane|/path\ntoo|many|fields\n";
        assert_eq!(
            parse_list_sessions_output(stdout),
            vec![TmuxSession {
                name: "lane".to_string(),
                path: "/path".to_string(),
            }]
        );
    }

    // --- window_creation_sequence ---

    #[test]
    fn window_sequence_uses_code_default_primary_with_no_extra_windows_when_unset() {
        assert_eq!(
            window_creation_sequence(&[], None),
            vec!["code".to_string()]
        );
    }

    #[test]
    fn window_sequence_puts_configured_primary_first_then_extra_windows_in_order() {
        let extra = vec!["shell".to_string()];
        assert_eq!(
            window_creation_sequence(&extra, Some("code")),
            vec!["code".to_string(), "shell".to_string()]
        );
    }

    #[test]
    fn window_sequence_respects_custom_primary_and_multiple_extras() {
        let extra = vec![
            "fish".to_string(),
            "claude".to_string(),
            "server".to_string(),
        ];
        assert_eq!(
            window_creation_sequence(&extra, Some("editor")),
            vec![
                "editor".to_string(),
                "fish".to_string(),
                "claude".to_string(),
                "server".to_string(),
            ]
        );
    }

    // --- FakeTmuxOps sequencing for session-with-windows creation ---

    #[test]
    fn fake_records_full_provisioning_sequence_for_a_new_session() {
        let fake = FakeTmuxOps::new();
        let name = "axiom-lane";
        let dir = "/repo/lane";
        let sequence =
            window_creation_sequence(&["fish".to_string(), "claude".to_string()], Some("code"));

        let (primary, extras) = sequence
            .split_first()
            .expect("sequence has a primary window");
        fake.new_session(name, dir, primary).unwrap();
        for window in extras {
            fake.new_window(name, window, dir).unwrap();
        }
        fake.select_window(name, primary).unwrap();

        assert_eq!(
            fake.calls(),
            vec![
                TmuxCall::NewSession {
                    name: name.to_string(),
                    dir: dir.to_string(),
                    primary_window: "code".to_string(),
                },
                TmuxCall::NewWindow {
                    name: name.to_string(),
                    window_name: "fish".to_string(),
                    dir: dir.to_string(),
                },
                TmuxCall::NewWindow {
                    name: name.to_string(),
                    window_name: "claude".to_string(),
                    dir: dir.to_string(),
                },
                TmuxCall::SelectWindow {
                    name: name.to_string(),
                    window: "code".to_string(),
                },
            ]
        );
    }

    #[test]
    fn fake_has_session_result_is_configurable() {
        let fake = FakeTmuxOps::new().with_has_session(Ok(true));
        assert!(fake.has_session("axiom-lane").unwrap());
        assert_eq!(
            fake.calls(),
            vec![TmuxCall::HasSession("axiom-lane".to_string())]
        );
    }

    #[test]
    fn fake_list_sessions_result_is_configurable() {
        let sessions = vec![TmuxSession {
            name: "axiom-lane".to_string(),
            path: "/repo/lane".to_string(),
        }];
        let fake = FakeTmuxOps::new().with_list_sessions(Ok(sessions.clone()));
        assert_eq!(fake.list_sessions().unwrap(), sessions);
        assert_eq!(fake.calls(), vec![TmuxCall::ListSessions]);
    }

    #[test]
    fn fake_attach_and_kill_session_are_recorded() {
        let fake = FakeTmuxOps::new();
        fake.attach("axiom-lane").unwrap();
        fake.kill_session("axiom-lane").unwrap();
        assert_eq!(
            fake.calls(),
            vec![
                TmuxCall::Attach("axiom-lane".to_string()),
                TmuxCall::KillSession("axiom-lane".to_string()),
            ]
        );
    }
}
