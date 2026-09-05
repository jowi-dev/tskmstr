//! Board-launched ticket-audit sessions: `docs/plans/board-audits.md`'s
//! "Launch" design.
//!
//! An audit session is an *interactive* `claude` conversation, unlike a
//! headless `tm work run` lane, so it is tmux-hosted: started detached via
//! [`TmuxOps::new_session_with_command`], attached on demand (the board's
//! job, not this module's). [`launch_audit`] is the whole launch sequence:
//! refuse to double-launch, pre-register a run row, then start the tmux
//! session running `claude` with the audit prompt.
//!
//! **Pre-registration and adoption.** [`launch_audit`] creates the run row
//! *before* `claude` has even started, with `pid = None` — there is no
//! wrapper process to report a pid the way `tm work run`'s supervisor does.
//! The session's first turn runs `tm ticket audit`, which reads
//! `TSKMSTR_SESSION_RUN_ID` (set here via `tmux -e`) and *adopts* the
//! pre-registered row: see [`crate::runs::session::register_session`]. That
//! closes the gap between "tmux session exists" and "the run is fully
//! wired up" without a second registration path.
//!
//! **Reap safety net.** If `claude` never boots (or the session dies before
//! adoption), the pid-`NULL` row simply goes stale and
//! [`crate::runs::RunStore::reap`] marks it failed once past the staleness
//! window — a visible failure rather than a silently orphaned run. Reap
//! staleness is measured in minutes; adoption happens on the session's
//! first turn, seconds after launch, so the gap is not a real race in
//! practice.

use std::path::Path;

use thiserror::Error;

use crate::agent::AgentRunner;
use crate::config::{AuditConfig, BackendIdentity};
use crate::runs::{RunStore, RunStoreError, StartRun};
use crate::work::naming::{expand_tilde, ticket_session_name};
use crate::work::tmux::{
    TmuxError, TmuxOps, has_live_window, session_window_names, unique_window_name,
};

/// Environment variable name the launched session's `claude` process
/// receives, carrying the pre-registered run id for
/// [`crate::runs::session::register_session`] to adopt. Deliberately
/// distinct from `TSKMSTR_RUN_ID` (see the module docs' "Pre-registration
/// and adoption" and `docs/plans/board-audits.md`'s ground truth on why
/// `TSKMSTR_RUN_ID` must stay lane-only): this variable is advisory input to
/// adoption, not a gate any existing hook reads.
pub const SESSION_RUN_ID_ENV: &str = "TSKMSTR_SESSION_RUN_ID";

/// Name of the tmux window a launched audit session's `claude` process runs
/// in. Public because it is also the board's liveness signal for audits (see
/// [`crate::work::tmux::TmuxOps::list_windows`]).
pub const AUDIT_WINDOW_NAME: &str = "audit";

/// Name of the plain-shell window every ticket session is provisioned with,
/// for `claude --resume`, manual git work, and running tests without leaving
/// the ticket's session.
pub const SHELL_WINDOW_NAME: &str = "shell";

/// Errors returned by [`launch_audit`].
#[derive(Debug, Error)]
pub enum AuditLaunchError {
    /// `[work.audit].dir` is unset, so there is nowhere to launch the
    /// session. Per `docs/plans/board-audits.md`'s "Launch" design, this is
    /// a status-line error for the caller to surface, not a crash.
    #[error("audit launching is not configured; set [work.audit].dir")]
    NotConfigured,

    /// This ticket's audit window is already live; launching again would
    /// double-run the audit. The caller should attach to `session_name`
    /// instead.
    ///
    /// Window-scoped, not session-scoped: the session collects a window per
    /// action taken against the ticket, so only a live window *named after
    /// this action* means the action is running (see
    /// [`crate::work::tmux::has_live_window`]).
    #[error("an audit session is already running: {session_name}:{window_name}")]
    AlreadyRunning {
        /// Name of the tmux session holding the live window.
        session_name: String,
        /// Name of the already-live window.
        window_name: String,
    },

    /// The run-state database could not be written to.
    #[error(transparent)]
    Store(#[from] RunStoreError),

    /// Shelling out to `tmux` failed.
    #[error(transparent)]
    Tmux(#[from] TmuxError),
}

/// Successful outcome of [`launch_audit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchOutcome {
    /// Id of the pre-registered run row (`kind = "audit"`); see the module
    /// docs' adoption story for why the row is created before `claude`
    /// starts.
    pub run_id: i64,
    /// Name of the tmux session the caller can attach to.
    pub session_name: String,
    /// Name of the window `claude` runs in: [`AUDIT_WINDOW_NAME`], or a
    /// [`unique_window_name`] suffixed variant when a previous audit's dead
    /// window still holds that name.
    pub window_name: String,
}

/// Substitutes `{key}` in `template` with `key`, producing the prompt text
/// handed to the agent on launch. `template` is already resolved by the
/// caller — [`launch_audit`] passes `audit_cfg.prompt`, falling back to
/// [`AgentRunner::default_audit_prompt_template`] when unset.
pub fn audit_prompt(template: &str, key: &str) -> String {
    template.replace("{key}", key)
}

/// Launches (or refuses to double-launch) a ticket-audit session for `key`.
///
/// 1. Errors with [`AuditLaunchError::NotConfigured`] if `audit_cfg.dir` is
///    unset.
/// 2. Errors with [`AuditLaunchError::AlreadyRunning`] if a live window
///    named [`AUDIT_WINDOW_NAME`] already exists in the ticket's
///    [`crate::work::naming::ticket_session_name`] session — no run row is created in this case, so a launch attempt
///    against an already-live audit never creates an orphaned
///    pre-registration.
/// 3. Otherwise pre-registers a run (`kind = "audit"`, `lane = "audit"`,
///    `pid = None`; see the module docs) and starts `claude <prompt>` (with
///    `--model` when `audit_cfg.model` is set — see
///    [`AgentRunner::interactive_shell_command`]) in
///    that window, with `SESSION_RUN_ID_ENV` set to the new run's id so the
///    in-session `tm ticket audit` can adopt it. The session (plus its
///    [`SHELL_WINDOW_NAME`] window) is created if this is the ticket's first
///    tmux-hosted action, and the window appended to it otherwise; the window
///    takes a [`unique_window_name`] suffix if a dead predecessor still holds
///    the plain name.
///
/// `home` resolves a leading `~` in `audit_cfg.dir` via
/// [`crate::work::naming::expand_tilde`], matching every other `~`-expanding
/// config caller in this codebase (config values are never expanded by the
/// `config` module itself).
///
/// `identity` is the invoking repo's [`BackendIdentity`]; its
/// [`session_slug`](BackendIdentity::session_slug) qualifies the ticket's
/// session name so same-numbered tickets in different repos never share a
/// session (GitHub issue #10).
///
/// `runner` is the AI coding agent this session launches (Claude today; see
/// [`AgentRunner`] and GitHub issue #17) — it supplies both the default
/// prompt template (when `audit_cfg.prompt` is unset) and the rendered
/// shell command.
pub fn launch_audit(
    store: &RunStore,
    tmux: &dyn TmuxOps,
    audit_cfg: &AuditConfig,
    home: &Path,
    identity: &BackendIdentity,
    runner: &dyn AgentRunner,
    key: &str,
) -> Result<LaunchOutcome, AuditLaunchError> {
    let raw_dir = audit_cfg
        .dir
        .as_deref()
        .ok_or(AuditLaunchError::NotConfigured)?;
    let dir = expand_tilde(raw_dir, home);
    let dir_str = dir.to_string_lossy().into_owned();

    let session_name = ticket_session_name(&identity.session_slug(), key);
    // One `list_windows` snapshot answers both questions — "is an audit
    // already running?" and "does the session exist yet?" — so the two
    // decisions cannot disagree about a session that appeared or vanished
    // between two probes.
    let windows = tmux.list_windows()?;
    if has_live_window(&windows, &session_name, AUDIT_WINDOW_NAME) {
        return Err(AuditLaunchError::AlreadyRunning {
            session_name,
            window_name: AUDIT_WINDOW_NAME.to_string(),
        });
    }
    let existing_windows = session_window_names(&windows, &session_name);
    let window_name = unique_window_name(AUDIT_WINDOW_NAME, &existing_windows);

    let run_id = store.start_run(&StartRun {
        ticket: key.to_string(),
        scope: identity.scope(),
        lane: "audit".to_string(),
        worktree: dir_str.clone(),
        branch: None,
        pid: None,
        kind: "audit".to_string(),
        log_path: None,
    })?;

    let template = audit_cfg
        .prompt
        .as_deref()
        .unwrap_or_else(|| runner.default_audit_prompt_template());
    let prompt = audit_prompt(template, key);
    let command = runner.interactive_shell_command(audit_cfg.model.as_deref(), &prompt);
    let env = [(SESSION_RUN_ID_ENV.to_string(), run_id.to_string())];

    if existing_windows.is_empty() {
        tmux.new_session_with_command(&session_name, &dir_str, &window_name, &env, &command)?;
        // The ticket's session is being created, so provision its shell
        // window too, then hand focus back to the action window `new_window`
        // just stole it from.
        tmux.new_window(&session_name, SHELL_WINDOW_NAME, &dir_str)?;
        tmux.select_window(&session_name, &window_name)?;
    } else {
        tmux.new_window_with_command(&session_name, &window_name, &dir_str, &env, &command)?;
    }

    Ok(LaunchOutcome {
        run_id,
        session_name,
        window_name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::claude::ClaudeRunner;
    use crate::runs::RunStatus;
    use crate::work::tmux::{FakeTmuxOps, TmuxCall, TmuxWindow};
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn open_store(dir: &Path) -> RunStore {
        RunStore::open(&dir.join("runs.db")).expect("open should succeed")
    }

    fn configured(dir: &str) -> AuditConfig {
        AuditConfig {
            dir: Some(dir.to_string()),
            prompt: None,
            model: None,
        }
    }

    /// Canonical test identity; its `session_slug()` is `proj`, so ticket
    /// sessions in these tests are named `tm-proj-<lowercased key>`.
    fn test_identity() -> crate::config::BackendIdentity {
        crate::config::BackendIdentity::Jira {
            base_url: "https://x.atlassian.net".to_string(),
            project_key: "PROJ".to_string(),
        }
    }

    /// The `command` string of the single `NewSessionWithCommand` call `tmux`
    /// recorded, for the tests that only care about how `claude` was invoked.
    fn launched_command(tmux: &FakeTmuxOps) -> Option<String> {
        tmux.calls().iter().find_map(|call| match call {
            TmuxCall::NewSessionWithCommand { command, .. }
            | TmuxCall::NewWindowWithCommand { command, .. } => Some(command.clone()),
            _ => None,
        })
    }

    #[test]
    fn audit_prompt_defaults_to_ticket_audit_template() {
        assert_eq!(
            audit_prompt(ClaudeRunner.default_audit_prompt_template(), "PROJ-1"),
            "/ticket-audit PROJ-1"
        );
    }

    #[test]
    fn audit_prompt_substitutes_key_in_custom_template() {
        assert_eq!(
            audit_prompt("/custom-audit for {key} please", "PROJ-1"),
            "/custom-audit for PROJ-1 please"
        );
    }

    #[test]
    fn launch_audit_creates_run_and_starts_tmux_session() {
        let db_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());
        let tmux = FakeTmuxOps::new();
        let home = PathBuf::from("/Users/jowi");
        let audit_cfg = configured("~/Projects/axiom");

        let outcome = launch_audit(
            &store,
            &tmux,
            &audit_cfg,
            &home,
            &test_identity(),
            &ClaudeRunner,
            "PROJ-1",
        )
        .expect("launch should succeed");

        assert_eq!(outcome.session_name, "tm-proj-proj-1");

        let run = store
            .run_by_id(outcome.run_id)
            .unwrap()
            .expect("run row should exist");
        assert_eq!(run.ticket, "PROJ-1");
        assert_eq!(run.kind, "audit");
        assert_eq!(run.lane, "audit");
        assert_eq!(run.status, RunStatus::Running);
        assert_eq!(run.pid, None);
        assert_eq!(run.worktree, "/Users/jowi/Projects/axiom");

        assert_eq!(outcome.window_name, "audit");
        assert_eq!(
            tmux.calls(),
            vec![
                TmuxCall::ListWindows,
                TmuxCall::NewSessionWithCommand {
                    name: "tm-proj-proj-1".to_string(),
                    dir: "/Users/jowi/Projects/axiom".to_string(),
                    window_name: "audit".to_string(),
                    env: vec![(SESSION_RUN_ID_ENV.to_string(), outcome.run_id.to_string())],
                    command: "claude '/ticket-audit PROJ-1'".to_string(),
                },
                TmuxCall::NewWindow {
                    name: "tm-proj-proj-1".to_string(),
                    window_name: SHELL_WINDOW_NAME.to_string(),
                    dir: "/Users/jowi/Projects/axiom".to_string(),
                },
                TmuxCall::SelectWindow {
                    name: "tm-proj-proj-1".to_string(),
                    window: "audit".to_string(),
                },
            ]
        );
    }

    #[test]
    fn launch_audit_errors_when_dir_is_unset() {
        let db_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());
        let tmux = FakeTmuxOps::new();
        let home = PathBuf::from("/Users/jowi");
        let audit_cfg = AuditConfig::default();

        let err = launch_audit(
            &store,
            &tmux,
            &audit_cfg,
            &home,
            &test_identity(),
            &ClaudeRunner,
            "PROJ-1",
        )
        .expect_err("should refuse to launch");

        assert!(matches!(err, AuditLaunchError::NotConfigured));
        assert!(store.list_runs().unwrap().is_empty());
        assert!(tmux.calls().is_empty());
    }

    /// A [`FakeTmuxOps`] whose `list_windows` snapshot is `windows`, each
    /// entry given as `(session, window, dead)`.
    fn tmux_with_windows(windows: &[(&str, &str, bool)]) -> FakeTmuxOps {
        FakeTmuxOps::new().with_list_windows(Ok(windows
            .iter()
            .map(|(session, name, dead)| TmuxWindow {
                session: (*session).to_string(),
                name: (*name).to_string(),
                dead: *dead,
            })
            .collect()))
    }

    #[test]
    fn launch_audit_errors_and_creates_no_run_when_the_audit_window_is_live() {
        let db_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());
        let tmux = tmux_with_windows(&[("tm-proj-proj-1", "audit", false)]);
        let home = PathBuf::from("/Users/jowi");
        let audit_cfg = configured("~/Projects/axiom");

        let err = launch_audit(
            &store,
            &tmux,
            &audit_cfg,
            &home,
            &test_identity(),
            &ClaudeRunner,
            "PROJ-1",
        )
        .expect_err("should refuse to double-launch");

        match err {
            AuditLaunchError::AlreadyRunning {
                session_name,
                window_name,
            } => {
                assert_eq!(session_name, "tm-proj-proj-1");
                assert_eq!(window_name, "audit");
            }
            other => panic!("expected AlreadyRunning, got {other:?}"),
        }
        assert!(
            store.list_runs().unwrap().is_empty(),
            "must not pre-register a run for an action that is already running"
        );
    }

    #[test]
    fn launch_audit_appends_a_window_when_the_ticket_session_already_exists() {
        // The session exists but holds no live `audit` window, so this is a
        // first audit against an already-touched ticket: append, don't
        // refuse, and don't try to create a duplicate session.
        let db_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());
        let tmux = tmux_with_windows(&[("tm-proj-proj-1", SHELL_WINDOW_NAME, false)]);
        let home = PathBuf::from("/Users/jowi");
        let audit_cfg = configured("/repo/axiom");

        let outcome = launch_audit(
            &store,
            &tmux,
            &audit_cfg,
            &home,
            &test_identity(),
            &ClaudeRunner,
            "PROJ-1",
        )
        .expect("launch should succeed");

        assert_eq!(outcome.window_name, "audit");
        assert_eq!(
            tmux.calls(),
            vec![
                TmuxCall::ListWindows,
                TmuxCall::NewWindowWithCommand {
                    name: "tm-proj-proj-1".to_string(),
                    window_name: "audit".to_string(),
                    dir: "/repo/axiom".to_string(),
                    env: vec![(SESSION_RUN_ID_ENV.to_string(), outcome.run_id.to_string())],
                    command: "claude '/ticket-audit PROJ-1'".to_string(),
                },
            ]
        );
    }

    #[test]
    fn launch_audit_suffixes_the_window_when_a_dead_one_holds_the_name() {
        // Windows are append-only and nothing renames them, so a relaunch
        // over dead aftermath takes the next free name.
        let db_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());
        let tmux = tmux_with_windows(&[("tm-proj-proj-1", "audit", true)]);
        let home = PathBuf::from("/Users/jowi");
        let audit_cfg = configured("/repo/axiom");

        let outcome = launch_audit(
            &store,
            &tmux,
            &audit_cfg,
            &home,
            &test_identity(),
            &ClaudeRunner,
            "PROJ-1",
        )
        .expect("launch should succeed");

        assert_eq!(outcome.window_name, "audit-2");
    }

    #[test]
    fn launch_audit_relaunches_over_a_dead_audit_window() {
        // A window whose pane exited is aftermath, not a running audit.
        let db_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());
        let tmux = tmux_with_windows(&[("tm-proj-proj-1", "audit", true)]);
        let home = PathBuf::from("/Users/jowi");
        let audit_cfg = configured("/repo/axiom");

        launch_audit(
            &store,
            &tmux,
            &audit_cfg,
            &home,
            &test_identity(),
            &ClaudeRunner,
            "PROJ-1",
        )
        .expect("launch should succeed");

        assert!(
            tmux.calls()
                .iter()
                .any(|call| matches!(call, TmuxCall::NewWindowWithCommand { .. })),
            "expected a window append, got {:?}",
            tmux.calls()
        );
    }

    #[test]
    fn launch_audit_uses_custom_prompt_template() {
        let db_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());
        let tmux = FakeTmuxOps::new();
        let home = PathBuf::from("/Users/jowi");
        let audit_cfg = AuditConfig {
            dir: Some("/repo/axiom".to_string()),
            prompt: Some("/custom-audit {key}".to_string()),
            model: None,
        };

        launch_audit(
            &store,
            &tmux,
            &audit_cfg,
            &home,
            &test_identity(),
            &ClaudeRunner,
            "PROJ-9",
        )
        .unwrap();

        assert_eq!(
            launched_command(&tmux),
            Some("claude '/custom-audit PROJ-9'".to_string())
        );
    }

    #[test]
    fn launch_audit_passes_configured_model_to_claude() {
        let db_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());
        let tmux = FakeTmuxOps::new();
        let home = PathBuf::from("/Users/jowi");
        let audit_cfg = AuditConfig {
            dir: Some("/repo/axiom".to_string()),
            prompt: None,
            model: Some("opus".to_string()),
        };

        launch_audit(
            &store,
            &tmux,
            &audit_cfg,
            &home,
            &test_identity(),
            &ClaudeRunner,
            "PROJ-9",
        )
        .unwrap();

        assert_eq!(
            launched_command(&tmux),
            Some("claude --model 'opus' '/ticket-audit PROJ-9'".to_string())
        );
    }
}
