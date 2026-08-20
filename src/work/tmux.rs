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

/// One row of `tmux list-windows -a` output: which session a window belongs
/// to, its name, and whether its pane has died.
///
/// The window *name* is the liveness signal for tmux-hosted actions (see
/// [`TmuxOps::list_windows`]), so `dead` matters: a window whose pane exited
/// can linger when `remain-on-exit` is set, and a lingering window is not a
/// running action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxWindow {
    /// `#{session_name}`.
    pub session: String,
    /// `#{window_name}`.
    pub name: String,
    /// `#{pane_dead}`: the window's pane has exited but the window survives.
    pub dead: bool,
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

    /// Create a new detached session named `name`, rooted at `dir`, with
    /// its first window named `window_name`, running `command` as that
    /// window's pane command instead of the default shell
    /// (`tmux new-session -d -s <name> -c <dir> -n <window_name> [-e
    /// KEY=VAL ...] <command>`).
    ///
    /// `env` becomes one `-e KEY=VAL` pair per entry — tmux ≥ 3.2's
    /// per-session environment flag (the nix-pinned tmux satisfies this).
    /// `command` is a single positional argument, not shell-wrapped by this
    /// method; see [`crate::work::audit::launch_audit`] for why its caller
    /// must itself produce a string tmux's own shell can parse (tmux hands
    /// a single trailing command string to the user's `$SHELL -c`).
    fn new_session_with_command(
        &self,
        name: &str,
        dir: &str,
        window_name: &str,
        env: &[(String, String)],
        command: &str,
    ) -> Result<(), TmuxError>;

    /// Create an additional window named `window_name` in session `name`,
    /// rooted at `dir` (`tmux new-window -t <name> -n <window_name> -c
    /// <dir>`).
    fn new_window(&self, name: &str, window_name: &str, dir: &str) -> Result<(), TmuxError>;

    /// Append a window named `window_name` to session `name`, rooted at
    /// `dir`, running `command` as its pane command instead of the default
    /// shell (`tmux new-window -a -t <name>:{end} -n <window_name> -c <dir>
    /// [-e KEY=VAL ...] <command>`).
    ///
    /// The window-with-a-command counterpart of
    /// [`TmuxOps::new_session_with_command`], for the second and later
    /// actions taken against a ticket whose session already exists; `env` and
    /// `command` carry the same contract (one `-e KEY=VAL` per entry;
    /// `command` is a single string tmux hands to `$SHELL -c`, so the caller
    /// escapes it).
    ///
    /// Appended at `{end}`, not after the session's current window, so window
    /// order is the ticket's action history — tmux's default insertion point
    /// follows whichever window happens to be selected.
    fn new_window_with_command(
        &self,
        name: &str,
        window_name: &str,
        dir: &str,
        env: &[(String, String)],
        command: &str,
    ) -> Result<(), TmuxError>;

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

    /// List every window of every running session
    /// (`tmux list-windows -a -F '#{session_name}:#{window_name}:#{pane_dead}'`).
    ///
    /// This is the liveness signal for tmux-hosted actions. Session
    /// existence cannot serve that role: one session holds a ticket's whole
    /// action history (`tm-<key>`, see [`crate::work::audit`]), so its
    /// existence only means "this ticket has been touched" — it is the
    /// presence of a live window *named after the action* that means the
    /// action is running.
    ///
    /// Tolerant in the same way [`TmuxOps::list_sessions`] is: no `tmux`
    /// server running yields `Ok(vec![])`, and malformed rows are dropped
    /// rather than erroring.
    fn list_windows(&self) -> Result<Vec<TmuxWindow>, TmuxError>;
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

/// The action a window name denotes: `name` with a trailing `-<n>` repeat
/// suffix (as produced by [`unique_window_name`]) stripped off.
///
/// Only an all-digit suffix is a repeat marker, so multi-word action names
/// like `bugbot-cleanup` survive intact.
pub fn window_action(name: &str) -> &str {
    match name.rsplit_once('-') {
        Some((base, suffix))
            if !base.is_empty()
                && !suffix.is_empty()
                && suffix.bytes().all(|b| b.is_ascii_digit()) =>
        {
            base
        }
        _ => name,
    }
}

/// The window name a new action window should take: `desired` if no window in
/// `existing` already holds it, otherwise `desired-2`, `desired-3`, … up to
/// the first free suffix.
///
/// Names, not indices, identify a ticket's action windows: `base-index` is
/// user-configurable, tmux renumbers indices when a window is killed, and
/// repeat actions (two fix passes) are expected. This is the whole naming
/// rule, kept pure so it is testable against a plain list of names — the
/// caller supplies `existing` from a [`TmuxOps::list_windows`] snapshot of
/// the ticket's session.
pub fn unique_window_name(desired: &str, existing: &[String]) -> String {
    if !existing.iter().any(|name| name == desired) {
        return desired.to_string();
    }
    (2..)
        .map(|n| format!("{desired}-{n}"))
        .find(|candidate| !existing.iter().any(|name| name == candidate))
        .expect("an unbounded counter always reaches a free name")
}

/// Whether `windows` (a [`TmuxOps::list_windows`] snapshot) holds a live
/// window for action `window_name` in session `session` — including a repeat
/// window like `fix-2`, whose action is still `fix` (see [`window_action`]).
///
/// The double-launch guard for tmux-hosted actions: a ticket's session
/// outlives any single action, so "is this action already running?" is a
/// question about a window, not a session. A window whose pane has died
/// (`remain-on-exit`) does not count as running — relaunching over that
/// aftermath is the intended behavior.
pub fn has_live_window(windows: &[TmuxWindow], session: &str, window_name: &str) -> bool {
    windows.iter().any(|window| {
        window.session == session && window_action(&window.name) == window_name && !window.dead
    })
}

/// Every window name in session `session`, in [`TmuxOps::list_windows`]
/// order — the input [`unique_window_name`] resolves a new action window's
/// name against, and, because a session always has at least one window, also
/// the create-session-vs-append-window fork: empty means the session does not
/// exist.
///
/// Deriving both from one snapshot (rather than a separate
/// [`TmuxOps::has_session`] probe) keeps them from disagreeing about a
/// session that appeared or vanished between two calls.
pub fn session_window_names(windows: &[TmuxWindow], session: &str) -> Vec<String> {
    windows
        .iter()
        .filter(|window| window.session == session)
        .map(|window| window.name.clone())
        .collect()
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

/// Builds the argv for [`TmuxOps::new_session_with_command`]: the same
/// `new-session -d -s <name> -c <dir> -n <window_name>` prefix as
/// [`new_session_args`], followed by one `-e KEY=VAL` pair per `env` entry
/// (in order), followed by `command` as the final positional argument.
fn new_session_with_command_args(
    name: &str,
    dir: &str,
    window_name: &str,
    env: &[(String, String)],
    command: &str,
) -> Vec<String> {
    let mut args = new_session_args(name, dir, window_name);
    for (key, value) in env {
        args.push("-e".to_string());
        args.push(format!("{key}={value}"));
    }
    args.push(command.to_string());
    args
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

/// Builds the argv for [`TmuxOps::new_window_with_command`]. Targets
/// `<name>:{end}` with `-a` so the window lands after the session's last
/// window (see that method's docs), then one `-e KEY=VAL` pair per `env`
/// entry, then `command` as the final positional argument.
fn new_window_with_command_args(
    name: &str,
    window_name: &str,
    dir: &str,
    env: &[(String, String)],
    command: &str,
) -> Vec<String> {
    let mut args = vec![
        "new-window".to_string(),
        "-a".to_string(),
        "-t".to_string(),
        format!("{name}:{{end}}"),
        "-n".to_string(),
        window_name.to_string(),
        "-c".to_string(),
        dir.to_string(),
    ];
    for (key, value) in env {
        args.push("-e".to_string());
        args.push(format!("{key}={value}"));
    }
    args.push(command.to_string());
    args
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

fn list_windows_args() -> Vec<String> {
    vec![
        "list-windows".to_string(),
        "-a".to_string(),
        "-F".to_string(),
        "#{session_name}:#{window_name}:#{pane_dead}".to_string(),
    ]
}

/// Parse `tmux list-windows -a -F '#{session_name}:#{window_name}:#{pane_dead}'`
/// output with the same tolerance [`parse_list_sessions_output`] has: drop
/// anything that isn't a well-formed row.
///
/// The session name is taken up to the *first* `:` and `pane_dead` from after
/// the *last* one, leaving everything between them as the window name — tmux
/// forbids `:` in session names but not in window names, and only the outer
/// two fields have fixed shapes.
fn parse_list_windows_output(stdout: &str) -> Vec<TmuxWindow> {
    stdout
        .lines()
        .filter_map(|line| {
            let (head, dead) = line.rsplit_once(':')?;
            let (session, name) = head.split_once(':')?;
            if session.is_empty() || name.is_empty() {
                return None;
            }
            let dead = match dead {
                "0" => false,
                "1" => true,
                _ => return None,
            };
            Some(TmuxWindow {
                session: session.to_string(),
                name: name.to_string(),
                dead,
            })
        })
        .collect()
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

    fn new_session_with_command(
        &self,
        name: &str,
        dir: &str,
        window_name: &str,
        env: &[(String, String)],
        command: &str,
    ) -> Result<(), TmuxError> {
        let output = run(
            "tmux new-session",
            &new_session_with_command_args(name, dir, window_name, env, command),
        )?;
        require_success("tmux new-session", &output)
    }

    fn new_window(&self, name: &str, window_name: &str, dir: &str) -> Result<(), TmuxError> {
        let output = run("tmux new-window", &new_window_args(name, window_name, dir))?;
        require_success("tmux new-window", &output)
    }

    fn new_window_with_command(
        &self,
        name: &str,
        window_name: &str,
        dir: &str,
        env: &[(String, String)],
        command: &str,
    ) -> Result<(), TmuxError> {
        let output = run(
            "tmux new-window",
            &new_window_with_command_args(name, window_name, dir, env, command),
        )?;
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

    fn list_windows(&self) -> Result<Vec<TmuxWindow>, TmuxError> {
        // Same no-server tolerance as `list_sessions`: `tmux list-windows -a`
        // exits non-zero with no server running, which is "no windows", not a
        // fault worth failing a badge refresh over.
        let output = run("tmux list-windows", &list_windows_args())?;
        Ok(parse_list_windows_output(&String::from_utf8_lossy(
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
    list_windows_result: std::cell::RefCell<Result<Vec<TmuxWindow>, TmuxError>>,
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
    /// `new_session_with_command(name, dir, window_name, env, command)`.
    NewSessionWithCommand {
        /// Session name.
        name: String,
        /// Session working directory.
        dir: String,
        /// First window name.
        window_name: String,
        /// Per-session environment pairs.
        env: Vec<(String, String)>,
        /// Pane command.
        command: String,
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
    /// `new_window_with_command(name, window_name, dir, env, command)`.
    NewWindowWithCommand {
        /// Session name.
        name: String,
        /// Window name.
        window_name: String,
        /// Window working directory.
        dir: String,
        /// Per-window environment pairs.
        env: Vec<(String, String)>,
        /// Pane command.
        command: String,
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
    /// `list_windows()`.
    ListWindows,
}

impl FakeTmuxOps {
    /// Create a fake where `has_session` returns `Ok(false)` and
    /// `list_sessions`/`list_windows` return `Ok(vec![])` unless overridden.
    pub fn new() -> Self {
        Self {
            has_session_result: std::cell::RefCell::new(Ok(false)),
            list_sessions_result: std::cell::RefCell::new(Ok(Vec::new())),
            list_windows_result: std::cell::RefCell::new(Ok(Vec::new())),
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

    /// Set the result `list_windows` will return.
    pub fn with_list_windows(self, result: Result<Vec<TmuxWindow>, TmuxError>) -> Self {
        *self.list_windows_result.borrow_mut() = result;
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

    fn new_session_with_command(
        &self,
        name: &str,
        dir: &str,
        window_name: &str,
        env: &[(String, String)],
        command: &str,
    ) -> Result<(), TmuxError> {
        self.calls
            .borrow_mut()
            .push(TmuxCall::NewSessionWithCommand {
                name: name.to_string(),
                dir: dir.to_string(),
                window_name: window_name.to_string(),
                env: env.to_vec(),
                command: command.to_string(),
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

    fn new_window_with_command(
        &self,
        name: &str,
        window_name: &str,
        dir: &str,
        env: &[(String, String)],
        command: &str,
    ) -> Result<(), TmuxError> {
        self.calls
            .borrow_mut()
            .push(TmuxCall::NewWindowWithCommand {
                name: name.to_string(),
                window_name: window_name.to_string(),
                dir: dir.to_string(),
                env: env.to_vec(),
                command: command.to_string(),
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

    fn list_windows(&self) -> Result<Vec<TmuxWindow>, TmuxError> {
        self.calls.borrow_mut().push(TmuxCall::ListWindows);
        self.list_windows_result.borrow().clone()
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
    fn new_session_with_command_args_with_no_env_appends_bare_command() {
        assert_eq!(
            new_session_with_command_args("tm-audit-proj-1", "/repo/axiom", "audit", &[], "claude"),
            vec![
                "new-session",
                "-d",
                "-s",
                "tm-audit-proj-1",
                "-c",
                "/repo/axiom",
                "-n",
                "audit",
                "claude"
            ]
        );
    }

    #[test]
    fn new_session_with_command_args_adds_one_e_flag_per_env_pair_in_order() {
        let env = vec![
            ("TSKMSTR_SESSION_RUN_ID".to_string(), "42".to_string()),
            ("OTHER".to_string(), "value".to_string()),
        ];
        assert_eq!(
            new_session_with_command_args(
                "tm-audit-proj-1",
                "/repo/axiom",
                "audit",
                &env,
                "claude '/ticket-audit PROJ-1'"
            ),
            vec![
                "new-session",
                "-d",
                "-s",
                "tm-audit-proj-1",
                "-c",
                "/repo/axiom",
                "-n",
                "audit",
                "-e",
                "TSKMSTR_SESSION_RUN_ID=42",
                "-e",
                "OTHER=value",
                "claude '/ticket-audit PROJ-1'"
            ]
        );
    }

    #[test]
    fn new_window_with_command_args_append_at_the_end_with_env_and_command() {
        let env = vec![("TSKMSTR_SESSION_RUN_ID".to_string(), "42".to_string())];
        assert_eq!(
            new_window_with_command_args(
                "tm-proj-1",
                "audit",
                "/repo/axiom",
                &env,
                "claude '/ticket-audit PROJ-1'"
            ),
            vec![
                "new-window",
                "-a",
                "-t",
                "tm-proj-1:{end}",
                "-n",
                "audit",
                "-c",
                "/repo/axiom",
                "-e",
                "TSKMSTR_SESSION_RUN_ID=42",
                "claude '/ticket-audit PROJ-1'"
            ]
        );
    }

    #[test]
    fn fake_records_new_window_with_command_call() {
        let fake = FakeTmuxOps::new();
        let env = vec![("TSKMSTR_SESSION_RUN_ID".to_string(), "7".to_string())];
        fake.new_window_with_command("tm-proj-1", "audit", "/repo/axiom", &env, "claude")
            .unwrap();

        assert_eq!(
            fake.calls(),
            vec![TmuxCall::NewWindowWithCommand {
                name: "tm-proj-1".to_string(),
                window_name: "audit".to_string(),
                dir: "/repo/axiom".to_string(),
                env,
                command: "claude".to_string(),
            }]
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

    #[test]
    fn list_windows_args_cover_every_session_with_liveness() {
        assert_eq!(
            list_windows_args(),
            vec![
                "list-windows",
                "-a",
                "-F",
                "#{session_name}:#{window_name}:#{pane_dead}"
            ]
        );
    }

    // --- list-windows output parsing ---

    #[test]
    fn parses_window_lines_with_liveness() {
        let stdout = "tm-proj-1:audit:0\ntm-proj-1:fix:1\naxiom-lane:code:0\n";
        assert_eq!(
            parse_list_windows_output(stdout),
            vec![
                TmuxWindow {
                    session: "tm-proj-1".to_string(),
                    name: "audit".to_string(),
                    dead: false,
                },
                TmuxWindow {
                    session: "tm-proj-1".to_string(),
                    name: "fix".to_string(),
                    dead: true,
                },
                TmuxWindow {
                    session: "axiom-lane".to_string(),
                    name: "code".to_string(),
                    dead: false,
                },
            ]
        );
    }

    #[test]
    fn empty_window_output_yields_no_windows() {
        assert_eq!(parse_list_windows_output(""), Vec::new());
    }

    #[test]
    fn malformed_window_lines_are_dropped_not_erroring() {
        // Same tolerance as `parse_list_sessions_output`: a line without the
        // two delimiters, an empty field, or a `pane_dead` that isn't tmux's
        // `0`/`1` flag is skipped rather than failing the whole listing.
        let stdout = "no-delimiters\ntm-proj-1:audit:0\n:empty-session:0\ntm-x:win:maybe\n";
        assert_eq!(
            parse_list_windows_output(stdout),
            vec![TmuxWindow {
                session: "tm-proj-1".to_string(),
                name: "audit".to_string(),
                dead: false,
            }]
        );
    }

    #[test]
    fn window_names_containing_a_colon_keep_their_colon() {
        // tmux does not forbid `:` in a window name, and only the first and
        // last fields of the format string are fixed, so the middle field is
        // whatever remains.
        assert_eq!(
            parse_list_windows_output("tm-proj-1:fix:pass:2:0\n"),
            vec![TmuxWindow {
                session: "tm-proj-1".to_string(),
                name: "fix:pass:2".to_string(),
                dead: false,
            }]
        );
    }

    #[test]
    fn fake_list_windows_result_is_configurable() {
        let windows = vec![TmuxWindow {
            session: "tm-proj-1".to_string(),
            name: "audit".to_string(),
            dead: false,
        }];
        let fake = FakeTmuxOps::new().with_list_windows(Ok(windows.clone()));
        assert_eq!(fake.list_windows().unwrap(), windows);
        assert_eq!(fake.calls(), vec![TmuxCall::ListWindows]);
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

    // --- unique_window_name / window_action ---

    #[test]
    fn unique_window_name_keeps_the_desired_name_when_unused() {
        assert_eq!(unique_window_name("fix", &["audit".to_string()]), "fix");
    }

    #[test]
    fn unique_window_name_suffixes_the_second_pass() {
        assert_eq!(
            unique_window_name("fix", &["audit".to_string(), "fix".to_string()]),
            "fix-2"
        );
    }

    #[test]
    fn unique_window_name_counts_up_past_every_taken_suffix() {
        let existing = vec![
            "fix".to_string(),
            "fix-2".to_string(),
            "fix-3".to_string(),
            "work".to_string(),
        ];
        assert_eq!(unique_window_name("fix", &existing), "fix-4");
    }

    #[test]
    fn unique_window_name_fills_a_gap_left_by_a_killed_window() {
        // Windows are append-only, but a killed one frees its name; reusing
        // the lowest free suffix keeps names short and is what tmux's own
        // window list would suggest.
        let existing = vec!["fix".to_string(), "fix-3".to_string()];
        assert_eq!(unique_window_name("fix", &existing), "fix-2");
    }

    #[test]
    fn unique_window_name_does_not_confuse_a_different_action() {
        assert_eq!(unique_window_name("fix", &["prefix".to_string()]), "fix");
    }

    #[test]
    fn window_action_strips_a_repeat_suffix() {
        assert_eq!(window_action("fix-2"), "fix");
        assert_eq!(window_action("fix"), "fix");
    }

    #[test]
    fn window_action_leaves_non_numeric_suffixes_alone() {
        // `bugbot-cleanup` is one action's name, not `bugbot` repeat
        // `cleanup`.
        assert_eq!(window_action("bugbot-cleanup"), "bugbot-cleanup");
        assert_eq!(window_action("fix-"), "fix-");
    }

    // --- window-set queries over a list_windows snapshot ---

    fn windows() -> Vec<TmuxWindow> {
        vec![
            TmuxWindow {
                session: "tm-proj-1".to_string(),
                name: "audit".to_string(),
                dead: false,
            },
            TmuxWindow {
                session: "tm-proj-1".to_string(),
                name: "fix".to_string(),
                dead: true,
            },
        ]
    }

    #[test]
    fn has_live_window_finds_a_live_window_by_session_and_name() {
        assert!(has_live_window(&windows(), "tm-proj-1", "audit"));
    }

    #[test]
    fn has_live_window_matches_a_repeat_windows_action() {
        // A second `fix` pass runs in `fix-2`; the action is still `fix`, so
        // the double-launch guard must see it.
        let windows = vec![TmuxWindow {
            session: "tm-proj-1".to_string(),
            name: "fix-2".to_string(),
            dead: false,
        }];
        assert!(has_live_window(&windows, "tm-proj-1", "fix"));
    }

    #[test]
    fn has_live_window_ignores_a_dead_window_of_that_name() {
        assert!(!has_live_window(&windows(), "tm-proj-1", "fix"));
    }

    #[test]
    fn has_live_window_ignores_a_same_named_window_in_another_session() {
        assert!(!has_live_window(&windows(), "tm-proj-2", "audit"));
    }

    #[test]
    fn session_window_names_lists_that_sessions_windows_including_dead_ones() {
        // A dead pane doesn't free the window's name, and doesn't remove the
        // session either — an empty list is exactly "no such session".
        assert_eq!(
            session_window_names(&windows(), "tm-proj-1"),
            vec!["audit".to_string(), "fix".to_string()]
        );
        assert!(session_window_names(&windows(), "tm-proj-2").is_empty());
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
    fn fake_records_new_session_with_command_call() {
        let fake = FakeTmuxOps::new();
        let env = vec![("TSKMSTR_SESSION_RUN_ID".to_string(), "7".to_string())];
        fake.new_session_with_command(
            "tm-audit-proj-1",
            "/repo/axiom",
            "audit",
            &env,
            "claude '/ticket-audit PROJ-1'",
        )
        .unwrap();

        assert_eq!(
            fake.calls(),
            vec![TmuxCall::NewSessionWithCommand {
                name: "tm-audit-proj-1".to_string(),
                dir: "/repo/axiom".to_string(),
                window_name: "audit".to_string(),
                env,
                command: "claude '/ticket-audit PROJ-1'".to_string(),
            }]
        );
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
