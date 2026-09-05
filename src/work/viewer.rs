//! Viewer windows for headless runs: issue #2 phase 4 of
//! `docs/plans/one-session-per-ticket.md`.
//!
//! A headless run's `claude` process belongs to a `setsid`'d supervisor, not
//! to tmux. [`crate::work::detach`]'s module docs spell out why that
//! supervisor exists and why it must survive `tm work run` returning the
//! terminal: it is the only thing left that will call
//! [`crate::runs::RunStore::finish_run`]. Hosting it in a tmux window would
//! hand its lifetime to the tmux server, which means `tmux kill-session`, a
//! server restart, or a reboot would silently destroy a run mid-flight and
//! leave its row stuck at `running` until `tm runs reap`.
//!
//! So a headless run still *gets* a window in the ticket's `tm-<scope>-<key>`
//! session — otherwise the session stops being the ticket's whole action
//! history — but the window is a **viewer**: it runs `tm runs logs <id>
//! --follow` and owns nothing. Two consequences follow, and both are the
//! point rather than limitations to fix later:
//!
//! - **A viewer is disposable.** Killing it, or killing the whole session,
//!   costs nothing but the tail. This is exactly what makes
//!   [`crate::cli::work::session`]'s reconstruction safe to run at any time.
//! - **Log files, not scrollback, are the archive.** tmux scrollback is
//!   capped by `history-limit` and dies with the server; the log file is what
//!   `tm runs logs` reads and what survives a reboot.
//!
//! # Mixed ownership is deliberate
//!
//! An interactive action owns its window ([`crate::work::interactive`]); a
//! headless action gets a viewer over a process that lives outside tmux
//! entirely. Two window kinds in one session looks like an inconsistency to
//! be tidied up; it is not. Making the viewer own the process would
//! re-introduce the destructive coupling above, and making the interactive
//! window a viewer would remove the steering that phase 3 existed to add.
//!
//! # Best-effort, never fatal
//!
//! By the time a viewer can be launched, the supervisor is already running:
//! the run is happening whether or not tmux cooperates. On a machine with no
//! tmux server, no `tmux` binary, or a session that vanished between the
//! `list_windows` snapshot and now, failing the command would report an error
//! for a run that is in fact fine. [`launch_viewer_window`] therefore returns
//! the `tmux` error for the caller to *report* rather than propagate — see
//! [`crate::cli::work::run`].

use std::path::Path;

use crate::work::audit::{SHELL_WINDOW_NAME, shell_quote};
use crate::work::interactive::ActionWindow;
use crate::work::tmux::{TmuxError, TmuxOps};

/// The shell command line a viewer window runs: `tm runs logs <run_id>
/// --follow`, with `tm_program` and every argument [`shell_quote`]d (tmux
/// hands the window's command to the user's `$SHELL -c`, so it must be valid
/// shell).
///
/// **Addressed by run id, not `<KEY> --kind <kind>`.** The issue's sketch
/// uses the ticket key plus a kind, but that resolves to "the *latest* run of
/// that kind", which is not necessarily this one: a repeat action (a second
/// `fix` pass) started while an earlier viewer is still open would leave the
/// older window silently following the newer run's log. The launcher always
/// knows the row id it just created, and `tm runs logs` accepts a numeric run
/// id in the same position, so the precise address costs nothing.
///
/// `tm_program` is the launching binary's own path
/// (`std::env::current_exe()`), not a bare `tm`: a viewer launched by a `tm`
/// that is not on `$PATH` — a `cargo run` build, or a nix store path — must
/// still be able to find it.
pub fn viewer_command(tm_program: &Path, run_id: i64) -> String {
    format!(
        "{} runs logs {run_id} --follow",
        shell_quote(&tm_program.to_string_lossy())
    )
}

/// Create `target`'s viewer window, following `run_id`'s log from `dir`.
///
/// Creates the ticket's session (plus its
/// [`crate::work::audit::SHELL_WINDOW_NAME`] window) when this is the
/// ticket's first tmux-hosted action, and appends a window otherwise, per
/// `target.session_exists` — the same fork
/// [`crate::work::interactive::launch_interactive_run`] makes.
///
/// Carries no `TSKMSTR_SESSION_RUN_ID`, and no environment at all: this
/// window runs `tm runs logs`, not `claude`, so there is no session to adopt
/// the run row. The supervisor already owns this run's lifecycle through
/// `TSKMSTR_RUN_ID`, and handing a second owner the same run is what the
/// whole env-var split exists to prevent (see
/// [`crate::agent::RunMode`]).
pub fn launch_viewer_window(
    tmux: &dyn TmuxOps,
    target: &ActionWindow,
    dir: &str,
    tm_program: &Path,
    run_id: i64,
) -> Result<(), TmuxError> {
    let command = viewer_command(tm_program, run_id);
    if target.session_exists {
        tmux.new_window_with_command(
            &target.session_name,
            &target.window_name,
            dir,
            &[],
            &command,
        )
    } else {
        tmux.new_session_with_command(
            &target.session_name,
            dir,
            &target.window_name,
            &[],
            &command,
        )?;
        // Creating the ticket's session, so provision its shell window too,
        // then hand focus back to the action window `new_window` just stole
        // it from.
        tmux.new_window(&target.session_name, SHELL_WINDOW_NAME, dir)?;
        tmux.select_window(&target.session_name, &target.window_name)
    }
}

/// [`launch_viewer_window`], with its outcome reported to `out` as part of a
/// launch summary instead of propagated.
///
/// A viewer is a convenience over a run that is *already going*: the
/// supervisor was spawned before this is called, and it owns the run's
/// lifecycle from there (see [`crate::work::detach`]). Turning a `tmux`
/// failure — no server, no binary, a session killed a moment ago — into a
/// command failure would report a broken run to the user and, worse, invite
/// them to re-run it. So the error is *printed*, naming what was lost, and
/// the command still succeeds.
///
/// Only a write to `out` can fail here.
pub fn launch_and_report_viewer(
    tmux: &dyn TmuxOps,
    target: &ActionWindow,
    dir: &str,
    tm_program: &Path,
    run_id: i64,
    out: &mut dyn std::io::Write,
) -> std::io::Result<()> {
    match launch_viewer_window(tmux, target, dir, tm_program, run_id) {
        Ok(()) => {
            writeln!(
                out,
                "window    {}:{} (log viewer)",
                target.session_name, target.window_name
            )?;
            writeln!(out, "attach:   tmux attach -t {}", target.session_name)
        }
        Err(err) => writeln!(
            out,
            "window    none — no log viewer ({err}); the run itself is unaffected"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::interactive::{WORK_WINDOW_NAME, resolve_action_window};
    use crate::work::tmux::{FakeTmuxOps, TmuxCall, TmuxSession, TmuxWindow};
    use std::path::PathBuf;

    /// A [`TmuxOps`] whose every mutating call fails, for the
    /// no-tmux-server/no-binary case [`launch_and_report_viewer`] must
    /// survive.
    struct BrokenTmuxOps;

    impl BrokenTmuxOps {
        fn error() -> TmuxError {
            TmuxError::Spawn {
                command: "tmux new-session".to_string(),
                message: "No such file or directory".to_string(),
            }
        }
    }

    impl TmuxOps for BrokenTmuxOps {
        fn has_session(&self, _name: &str) -> Result<bool, TmuxError> {
            Ok(false)
        }
        fn new_session(&self, _: &str, _: &str, _: &str) -> Result<(), TmuxError> {
            Err(Self::error())
        }
        fn new_session_with_command(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &[(String, String)],
            _: &str,
        ) -> Result<(), TmuxError> {
            Err(Self::error())
        }
        fn new_window(&self, _: &str, _: &str, _: &str) -> Result<(), TmuxError> {
            Err(Self::error())
        }
        fn new_window_with_command(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &[(String, String)],
            _: &str,
        ) -> Result<(), TmuxError> {
            Err(Self::error())
        }
        fn select_window(&self, _: &str, _: &str) -> Result<(), TmuxError> {
            Err(Self::error())
        }
        fn attach(&self, _: &str) -> Result<crate::work::tmux::AttachOutcome, TmuxError> {
            Err(Self::error())
        }
        fn kill_session(&self, _: &str) -> Result<(), TmuxError> {
            Err(Self::error())
        }
        fn list_sessions(&self) -> Result<Vec<TmuxSession>, TmuxError> {
            Ok(Vec::new())
        }
        fn list_windows(&self) -> Result<Vec<TmuxWindow>, TmuxError> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn launch_and_report_viewer_reports_the_window_and_how_to_attach() {
        let target = resolve_action_window(&[], "proj", "PROJ-1", WORK_WINDOW_NAME).unwrap();
        let tmux = FakeTmuxOps::new();
        let mut out = Vec::new();

        launch_and_report_viewer(&tmux, &target, "/wt", Path::new("/bin/tm"), 9, &mut out).unwrap();

        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("window    tm-proj-proj-1:work (log viewer)"),
            "{text}"
        );
        assert!(
            text.contains("attach:   tmux attach -t tm-proj-proj-1"),
            "{text}"
        );
    }

    /// The whole reason this wrapper exists: the supervisor is already
    /// running by now, so a tmux failure must not become a command failure.
    #[test]
    fn launch_and_report_viewer_reports_a_tmux_failure_without_failing() {
        let target = resolve_action_window(&[], "proj", "PROJ-1", WORK_WINDOW_NAME).unwrap();
        let mut out = Vec::new();

        launch_and_report_viewer(
            &BrokenTmuxOps,
            &target,
            "/wt",
            Path::new("/bin/tm"),
            9,
            &mut out,
        )
        .expect("a tmux failure must not fail the command");

        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("no log viewer"), "{text}");
        assert!(text.contains("the run itself is unaffected"), "{text}");
    }

    #[test]
    fn viewer_command_follows_the_runs_log_by_id() {
        let command = viewer_command(Path::new("/usr/local/bin/tm"), 42);

        assert_eq!(command, "'/usr/local/bin/tm' runs logs 42 --follow");
    }

    #[test]
    fn viewer_command_quotes_a_program_path_with_a_space() {
        let command = viewer_command(Path::new("/Applications/My Tools/tm"), 7);

        assert!(command.starts_with("'/Applications/My Tools/tm' runs logs 7"));
    }

    #[test]
    fn launch_viewer_window_creates_the_session_with_a_shell_window() {
        let target = resolve_action_window(&[], "proj", "PROJ-1", WORK_WINDOW_NAME).unwrap();
        let tmux = FakeTmuxOps::new();

        launch_viewer_window(&tmux, &target, "/wt/proj-1", Path::new("/bin/tm"), 9).unwrap();

        assert_eq!(
            tmux.calls(),
            vec![
                TmuxCall::NewSessionWithCommand {
                    name: "tm-proj-proj-1".to_string(),
                    dir: "/wt/proj-1".to_string(),
                    window_name: "work".to_string(),
                    env: Vec::new(),
                    command: "'/bin/tm' runs logs 9 --follow".to_string(),
                },
                TmuxCall::NewWindow {
                    name: "tm-proj-proj-1".to_string(),
                    window_name: "shell".to_string(),
                    dir: "/wt/proj-1".to_string(),
                },
                TmuxCall::SelectWindow {
                    name: "tm-proj-proj-1".to_string(),
                    window: "work".to_string(),
                },
            ]
        );
    }

    #[test]
    fn launch_viewer_window_appends_to_an_existing_ticket_session() {
        let windows = vec![TmuxWindow {
            session: "tm-proj-proj-1".to_string(),
            name: "audit".to_string(),
            dead: true,
        }];
        let target = resolve_action_window(&windows, "proj", "PROJ-1", WORK_WINDOW_NAME).unwrap();
        let tmux = FakeTmuxOps::new();

        launch_viewer_window(&tmux, &target, "/wt/proj-1", Path::new("/bin/tm"), 9).unwrap();

        assert_eq!(
            tmux.calls(),
            vec![TmuxCall::NewWindowWithCommand {
                name: "tm-proj-proj-1".to_string(),
                window_name: "work".to_string(),
                dir: "/wt/proj-1".to_string(),
                env: Vec::new(),
                command: "'/bin/tm' runs logs 9 --follow".to_string(),
            }]
        );
    }

    /// The viewer must never carry a run id in the environment. The
    /// supervisor owns this run through `TSKMSTR_RUN_ID`, and
    /// `TSKMSTR_SESSION_RUN_ID` would invite a `claude` session that happened
    /// to start in this window to adopt a row that is already owned.
    #[test]
    fn launch_viewer_window_passes_no_run_id_environment() {
        let target = resolve_action_window(&[], "proj", "PROJ-1", WORK_WINDOW_NAME).unwrap();
        let tmux = FakeTmuxOps::new();

        launch_viewer_window(&tmux, &target, "/wt/proj-1", &PathBuf::from("/bin/tm"), 9).unwrap();

        for call in tmux.calls() {
            if let TmuxCall::NewSessionWithCommand { env, .. }
            | TmuxCall::NewWindowWithCommand { env, .. } = call
            {
                assert!(env.is_empty(), "a viewer owns no run: {env:?}");
            }
        }
    }
}
