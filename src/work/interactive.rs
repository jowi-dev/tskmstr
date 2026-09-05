//! tmux-hosted interactive work and fix runs: issue #2 phase 3 of
//! `docs/plans/one-session-per-ticket.md`.
//!
//! `tm work run` and `tm review fix` used to have exactly one shape — a
//! `setsid`'d supervisor driving `claude -p`, watchable only through its log
//! file. This module is the other shape, and now the default: the run's
//! `claude` process lives in a named window of the ticket's `tm-<scope>-<key>`
//! session, so it can be attached to and steered mid-run.
//!
//! The lifecycle is [`crate::work::audit::launch_audit`]'s, generalized:
//!
//! 1. Pre-register the run row with `pid = None` (whoever calls this has
//!    already done that — [`crate::work::run::prepare_run_lane`] or
//!    [`crate::work::run::prepare_review_fix`]).
//! 2. Launch the window with the run id in `TSKMSTR_SESSION_RUN_ID` (via
//!    `tmux -e`), *never* `TSKMSTR_RUN_ID` — see
//!    [`crate::agent::RunMode`] for why that distinction is
//!    load-bearing and symptomless when wrong.
//! 3. The session adopts the pre-registered row on its first turn, through
//!    [`crate::runs::session::register_session`].
//! 4. `hooks/tm-session-end.sh` finishes the run when the Claude Code
//!    session ends.
//!
//! # Guard before side effects, not at launch time
//!
//! `launch_audit` can take one `list_windows` snapshot, refuse a
//! double-launch, and only then create its run row. A work run cannot: by
//! the time it has a [`crate::work::run::PreparedRun`] to launch it has
//! already provisioned a worktree, cut a branch, and started a run row. So
//! the snapshot and the refusal live in [`resolve_action_window`], which the
//! caller runs *before* preparing anything, and [`launch_interactive_run`]
//! consumes its [`ActionWindow`] verdict. One snapshot still answers both
//! "is this action already running?" and "does the session exist yet?".
//!
//! # Why the prompt goes through a file
//!
//! tmux takes the window's command as a single string and hands it to the
//! user's `$SHELL -c`. A fix prompt embeds the entire `vdiff
//! --export-comments` markdown and has no length bound;
//! [`crate::work::audit::shell_quote`] escapes correctly but cannot shorten
//! anything, and `ARG_MAX` is a real ceiling. So the prompt is written to a
//! file next to the run's other state and the command reads it back with
//! `"$(cat '<path>')"` — the same trick `work.ml` used for its `-p` value.

use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::agent::AgentInvocation;
use crate::work::audit::{SESSION_RUN_ID_ENV, SHELL_WINDOW_NAME, shell_quote};
use crate::work::naming::ticket_session_name;
use crate::work::run::PreparedRun;
use crate::work::tmux::{
    TmuxError, TmuxOps, TmuxWindow, has_live_window, session_window_names, unique_window_name,
};

/// Window name for an interactive `tm work run`.
pub const WORK_WINDOW_NAME: &str = "work";

/// Window name for an interactive `tm review fix` pass. Repeat passes become
/// `fix-2`, `fix-3`, … via [`unique_window_name`].
pub const FIX_WINDOW_NAME: &str = "fix";

/// Errors from [`resolve_action_window`] and [`launch_interactive_run`].
#[derive(Debug, Error)]
pub enum InteractiveLaunchError {
    /// A live window for this action already exists in the ticket's session,
    /// so a second `claude` session would be editing the same worktree
    /// concurrently. Window-scoped like
    /// [`crate::work::audit::AuditLaunchError::AlreadyRunning`]: the session
    /// holds the ticket's whole action history, so only a live window
    /// *named after this action* means the action is running.
    #[error("a {window_name} run is already live in {session_name}")]
    AlreadyRunning {
        /// Name of the tmux session holding the live window.
        session_name: String,
        /// Name of the already-live window.
        window_name: String,
    },

    /// Shelling out to `tmux` failed.
    #[error(transparent)]
    Tmux(#[from] TmuxError),

    /// Writing the prompt file failed.
    #[error("failed to write prompt file {path}: {source}")]
    PromptFile {
        /// The prompt file that could not be written.
        path: PathBuf,
        /// The underlying I/O error.
        source: io::Error,
    },
}

/// Where an interactive run's `claude` process is about to be hosted: the
/// verdict [`resolve_action_window`] reached from one `list_windows`
/// snapshot, consumed by [`launch_interactive_run`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionWindow {
    /// The ticket's session (`tm-<scope slug>-<lowercased key>`, see
    /// [`ticket_session_name`]).
    pub session_name: String,
    /// The window this run's `claude` will run in: the plain action name, or
    /// a [`unique_window_name`] suffixed variant when a previous attempt's
    /// window still holds it.
    pub window_name: String,
    /// Whether the session already exists — i.e. whether this run appends a
    /// window or creates the ticket's session. A session always has at
    /// least one window, so an empty window list means "no session".
    pub session_exists: bool,
}

/// Decide which window of `session_key`'s ticket session an interactive
/// `action` run should take, refusing with
/// [`InteractiveLaunchError::AlreadyRunning`] if one is already live.
///
/// `scope_slug` is the current repo's
/// [`crate::config::BackendIdentity::session_slug`], qualifying the session
/// name so same-numbered tickets in different repos never share a session
/// (GitHub issue #10).
///
/// Pure over a [`TmuxOps::list_windows`] snapshot. Call it *before*
/// provisioning anything: unlike an audit launch, a work run's caller has
/// already cut a branch and started a run row by the time it can launch, so
/// the refusal has to come first or it leaves both behind.
pub fn resolve_action_window(
    windows: &[TmuxWindow],
    scope_slug: &str,
    session_key: &str,
    action: &str,
) -> Result<ActionWindow, InteractiveLaunchError> {
    let session_name = ticket_session_name(scope_slug, session_key);
    if has_live_window(windows, &session_name, action) {
        return Err(InteractiveLaunchError::AlreadyRunning {
            session_name,
            window_name: action.to_string(),
        });
    }
    let existing = session_window_names(windows, &session_name);
    Ok(ActionWindow {
        window_name: unique_window_name(action, &existing),
        session_exists: !existing.is_empty(),
        session_name,
    })
}

/// The shell command line a tmux window runs for `invocation`, reading the
/// prompt back from `prompt_file`:
///
/// ```text
/// env -u ANTHROPIC_API_KEY -u ANTHROPIC_AUTH_TOKEN -u CLAUDECODE \
///   claude "$(cat '<prompt_file>')" '--model' 'fable' '--settings' '<path>' \
///   '--permission-mode' 'acceptEdits'
/// ```
///
/// Flag names are quoted along with their values — [`shell_quote`] is applied
/// uniformly rather than trying to recognize which arguments are flags.
///
/// Two things this must not lose:
///
/// - **The `env -u` prefix.** [`AgentInvocation::env_remove`] is
///   billing-safety critical and there is no `tmux` flag that *unsets* an
///   environment variable (`-e` only sets), so it has to be re-expressed as
///   `env -u` inside the command string. See that field's doc comment.
/// - **The double quotes around `$(cat ...)`.** Unquoted, the shell would
///   word-split the prompt into hundreds of arguments.
///
/// `invocation.args[0]` is the prompt under
/// [`crate::agent::RunMode::Interactive`] (the prompt is positional
/// there), and it is what gets replaced by the `cat`; every later argument
/// is passed through [`shell_quote`]d.
pub fn tmux_command_line(invocation: &AgentInvocation, prompt_file: &Path) -> String {
    let mut parts = vec!["env".to_string()];
    for var in &invocation.env_remove {
        parts.push("-u".to_string());
        parts.push(var.clone());
    }
    parts.push(invocation.program.clone());
    parts.push(format!(
        "\"$(cat {})\"",
        shell_quote(&prompt_file.to_string_lossy())
    ));
    for arg in invocation.args.iter().skip(1) {
        parts.push(shell_quote(arg));
    }
    parts.join(" ")
}

/// Instructions prepended to an interactive run's prompt, telling the
/// session to adopt its pre-registered run row on its first turn.
///
/// This is the one piece of the adoption chain that cannot be arranged from
/// outside the session: `register_session` needs `CLAUDE_CODE_SESSION_ID`,
/// which only exists *inside* a running Claude Code session, so something in
/// the session has to make the call. A board-launched audit gets this for
/// free — its prompt is `/ticket-audit <KEY>`, and that skill runs `tm
/// ticket audit` as its first step. Work and fix runs have no such skill in
/// front of them, so the instruction is stated outright.
///
/// Adoption is telemetry, not the work: if the session skips this line, the
/// run still happens and the row is eventually reaped stale rather than
/// finished.
pub fn registration_preamble(kind: &str, ticket: &str) -> String {
    format!(
        "First, before anything else, run this exact command to register this \
         session against its tracked run:\n\n    tm runs register --kind \
         {kind} {ticket}\n\nThen carry out the task below.\n\n"
    )
}

/// An interactive run's full prompt: [`registration_preamble`] followed by
/// `body`, the prompt the run would have had headlessly.
pub fn interactive_prompt(kind: &str, ticket: &str, body: &str) -> String {
    format!("{}{body}", registration_preamble(kind, ticket))
}

/// Launch `prepared`'s `claude` process in `target`, rooted in the run's
/// worktree.
///
/// Writes `prepared.invocation`'s prompt (its positional first argument) to
/// `prompt_path` and starts a window running [`tmux_command_line`] against
/// that file, with `SESSION_RUN_ID_ENV` carrying the pre-registered run id
/// for the session to adopt.
///
/// Creates the ticket's session — plus the worktree-rooted
/// [`SHELL_WINDOW_NAME`] window every ticket session gets — when this is its
/// first tmux-hosted action, and appends a window to it otherwise, per
/// `target.session_exists`.
pub fn launch_interactive_run(
    tmux: &dyn TmuxOps,
    target: &ActionWindow,
    prepared: &PreparedRun,
    prompt_path: &Path,
) -> Result<(), InteractiveLaunchError> {
    let prompt = prepared
        .invocation
        .args
        .first()
        .cloned()
        .unwrap_or_default();
    if let Some(parent) = prompt_path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| InteractiveLaunchError::PromptFile {
            path: prompt_path.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(prompt_path, prompt).map_err(|source| InteractiveLaunchError::PromptFile {
        path: prompt_path.to_path_buf(),
        source,
    })?;

    let command = tmux_command_line(&prepared.invocation, prompt_path);
    let dir = prepared.worktree.to_string_lossy().into_owned();
    let env = [(SESSION_RUN_ID_ENV.to_string(), prepared.run_id.to_string())];

    if target.session_exists {
        tmux.new_window_with_command(
            &target.session_name,
            &target.window_name,
            &dir,
            &env,
            &command,
        )?;
    } else {
        tmux.new_session_with_command(
            &target.session_name,
            &dir,
            &target.window_name,
            &env,
            &command,
        )?;
        // Creating the ticket's session, so provision its shell window too,
        // then hand focus back to the action window `new_window` just stole
        // it from.
        tmux.new_window(&target.session_name, SHELL_WINDOW_NAME, &dir)?;
        tmux.select_window(&target.session_name, &target.window_name)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::claude::ClaudeRunner;
    use crate::agent::{AgentRunner, InvocationInputs, RunMode};
    use crate::work::tmux::{FakeTmuxOps, TmuxCall};
    use tempfile::tempdir;

    fn window(session: &str, name: &str, dead: bool) -> TmuxWindow {
        TmuxWindow {
            session: session.to_string(),
            name: name.to_string(),
            dead,
        }
    }

    fn interactive_invocation(prompt: &str) -> AgentInvocation {
        ClaudeRunner.build_invocation(InvocationInputs {
            prompt: prompt.to_string(),
            model: Some("fable".to_string()),
            max_turns: Some("200".to_string()),
            permission_mode: Some("acceptEdits".to_string()),
            settings_path: PathBuf::from("/hooks/settings.json"),
            run_id: Some("7".to_string()),
            mode: RunMode::Interactive,
        })
    }

    fn prepared(worktree: &Path, prompt: &str) -> PreparedRun {
        PreparedRun {
            run_id: 7,
            lane: "mylane".to_string(),
            ticket: Some("PROJ-1".to_string()),
            wt_name: "proj-1".to_string(),
            timestamp: "20260820-120000".to_string(),
            worktree: worktree.to_path_buf(),
            branch: "jowi-dev/proj-1-slug".to_string(),
            invocation: interactive_invocation(prompt),
            out_json_path: PathBuf::from("/state/proj-1-20260820-120000.json"),
        }
    }

    #[test]
    fn resolve_action_window_creates_the_session_when_the_ticket_has_none() {
        let target = resolve_action_window(&[], "proj", "PROJ-1", WORK_WINDOW_NAME).unwrap();

        assert_eq!(
            target,
            ActionWindow {
                session_name: "tm-proj-proj-1".to_string(),
                window_name: "work".to_string(),
                session_exists: false,
            }
        );
    }

    #[test]
    fn resolve_action_window_appends_to_an_existing_ticket_session() {
        let windows = vec![
            window("tm-proj-proj-1", "audit", true),
            window("tm-proj-proj-1", "shell", false),
        ];

        let target = resolve_action_window(&windows, "proj", "proj-1", WORK_WINDOW_NAME).unwrap();

        assert_eq!(target.window_name, "work");
        assert!(target.session_exists);
    }

    #[test]
    fn resolve_action_window_suffixes_past_a_dead_predecessor() {
        let windows = vec![
            window("tm-proj-proj-1", "fix", true),
            window("tm-proj-proj-1", "shell", false),
        ];

        let target = resolve_action_window(&windows, "proj", "PROJ-1", FIX_WINDOW_NAME).unwrap();

        assert_eq!(target.window_name, "fix-2");
    }

    #[test]
    fn resolve_action_window_refuses_while_the_action_is_live() {
        let windows = vec![window("tm-proj-proj-1", "work", false)];

        let err = resolve_action_window(&windows, "proj", "PROJ-1", WORK_WINDOW_NAME).unwrap_err();

        match err {
            InteractiveLaunchError::AlreadyRunning {
                session_name,
                window_name,
            } => {
                assert_eq!(session_name, "tm-proj-proj-1");
                assert_eq!(window_name, "work");
            }
            other => panic!("expected AlreadyRunning, got {other:?}"),
        }
    }

    #[test]
    fn resolve_action_window_refuses_while_a_repeat_of_the_action_is_live() {
        // `fix-2`'s action is still `fix` (see `tmux::window_action`), so a
        // live second pass blocks a third.
        let windows = vec![window("tm-proj-proj-1", "fix-2", false)];

        let err = resolve_action_window(&windows, "proj", "PROJ-1", FIX_WINDOW_NAME).unwrap_err();

        assert!(matches!(err, InteractiveLaunchError::AlreadyRunning { .. }));
    }

    #[test]
    fn resolve_action_window_ignores_another_tickets_live_window() {
        let windows = vec![window("tm-proj-proj-2", "work", false)];

        let target = resolve_action_window(&windows, "proj", "PROJ-1", WORK_WINDOW_NAME).unwrap();

        assert_eq!(target.session_name, "tm-proj-proj-1");
        assert!(!target.session_exists);
    }

    #[test]
    fn tmux_command_line_strips_billing_env_and_reads_the_prompt_from_the_file() {
        let invocation = interactive_invocation("do the thing");

        let command = tmux_command_line(&invocation, Path::new("/state/proj-1.prompt.md"));

        assert_eq!(
            command,
            "env -u ANTHROPIC_API_KEY -u ANTHROPIC_AUTH_TOKEN -u CLAUDECODE claude \
             \"$(cat '/state/proj-1.prompt.md')\" '--model' 'fable' '--settings' \
             '/hooks/settings.json' '--permission-mode' 'acceptEdits'"
        );
        assert!(
            !command.contains("do the thing"),
            "the prompt itself must never reach the command string — it is unbounded"
        );
    }

    #[test]
    fn tmux_command_line_quotes_a_prompt_path_with_a_quote_in_it() {
        let invocation = interactive_invocation("prompt");

        let command = tmux_command_line(&invocation, Path::new("/state/o'brien.prompt.md"));

        assert!(command.contains(r#""$(cat '/state/o'\''brien.prompt.md')""#));
    }

    #[test]
    fn registration_preamble_names_the_kind_and_ticket_to_register() {
        let preamble = registration_preamble("review-fix", "PROJ-1");

        assert!(preamble.contains("tm runs register --kind review-fix PROJ-1"));
    }

    #[test]
    fn interactive_prompt_keeps_the_body_after_the_preamble() {
        let prompt = interactive_prompt("lane", "PROJ-1", "Address every review comment");

        assert!(prompt.starts_with("First, before anything else"));
        assert!(prompt.ends_with("Address every review comment"));
    }

    #[test]
    fn launch_interactive_run_creates_the_session_with_a_shell_window() {
        let tmp = tempdir().unwrap();
        let worktree = tmp.path().join("Worktrees/axiom/proj-1");
        let prompt_path = tmp.path().join("state/proj-1.prompt.md");
        let prepared = prepared(&worktree, "do the thing");
        let target = resolve_action_window(&[], "proj", "PROJ-1", WORK_WINDOW_NAME).unwrap();
        let tmux = FakeTmuxOps::new();

        launch_interactive_run(&tmux, &target, &prepared, &prompt_path).unwrap();

        let dir = worktree.to_string_lossy().into_owned();
        let command = tmux_command_line(&prepared.invocation, &prompt_path);
        assert_eq!(
            tmux.calls(),
            vec![
                TmuxCall::NewSessionWithCommand {
                    name: "tm-proj-proj-1".to_string(),
                    dir: dir.clone(),
                    window_name: "work".to_string(),
                    env: vec![("TSKMSTR_SESSION_RUN_ID".to_string(), "7".to_string())],
                    command,
                },
                TmuxCall::NewWindow {
                    name: "tm-proj-proj-1".to_string(),
                    window_name: "shell".to_string(),
                    dir,
                },
                TmuxCall::SelectWindow {
                    name: "tm-proj-proj-1".to_string(),
                    window: "work".to_string(),
                },
            ]
        );
        assert_eq!(
            std::fs::read_to_string(&prompt_path).unwrap(),
            "do the thing"
        );
    }

    #[test]
    fn launch_interactive_run_appends_a_window_to_a_live_ticket_session() {
        let tmp = tempdir().unwrap();
        let worktree = tmp.path().join("Worktrees/axiom/proj-1");
        let prompt_path = tmp.path().join("state/proj-1.prompt.md");
        let prepared = prepared(&worktree, "fix the comments");
        let windows = vec![window("tm-proj-proj-1", "audit", true)];
        let target = resolve_action_window(&windows, "proj", "PROJ-1", FIX_WINDOW_NAME).unwrap();
        let tmux = FakeTmuxOps::new();

        launch_interactive_run(&tmux, &target, &prepared, &prompt_path).unwrap();

        match tmux.calls().as_slice() {
            [
                TmuxCall::NewWindowWithCommand {
                    name,
                    window_name,
                    dir,
                    env,
                    ..
                },
            ] => {
                assert_eq!(name, "tm-proj-proj-1");
                assert_eq!(window_name, "fix");
                assert_eq!(dir, &worktree.to_string_lossy().into_owned());
                assert_eq!(
                    env,
                    &vec![("TSKMSTR_SESSION_RUN_ID".to_string(), "7".to_string())]
                );
            }
            other => panic!("expected a single NewWindowWithCommand, got {other:?}"),
        }
    }

    /// The one env-var assertion that matters at the tmux seam: whatever
    /// [`crate::agent::claude::ClaudeRunner::build_invocation`] decided,
    /// `TSKMSTR_RUN_ID` must never reach a tmux-hosted window — it would
    /// gate off the SessionEnd hook that is the only thing left to finish
    /// the run. See [`crate::agent::RunMode`].
    #[test]
    fn launch_interactive_run_never_passes_the_supervisor_owned_run_id_var() {
        let tmp = tempdir().unwrap();
        let prepared = prepared(&tmp.path().join("wt"), "do the thing");
        let target = resolve_action_window(&[], "proj", "PROJ-1", WORK_WINDOW_NAME).unwrap();
        let tmux = FakeTmuxOps::new();

        launch_interactive_run(&tmux, &target, &prepared, &tmp.path().join("p.md")).unwrap();

        let env = tmux
            .calls()
            .iter()
            .find_map(|call| match call {
                TmuxCall::NewSessionWithCommand { env, .. }
                | TmuxCall::NewWindowWithCommand { env, .. } => Some(env.clone()),
                _ => None,
            })
            .expect("a window was launched");
        assert!(
            env.iter().all(|(key, _)| key != "TSKMSTR_RUN_ID"),
            "TSKMSTR_RUN_ID means a supervisor owns finish; nothing owns an \
             interactive run but its own SessionEnd hook"
        );
        assert!(env.iter().any(|(key, _)| key == "TSKMSTR_SESSION_RUN_ID"));
    }
}
