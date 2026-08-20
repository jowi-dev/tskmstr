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

use crate::config::AuditConfig;
use crate::runs::{RunStore, RunStoreError, StartRun};
use crate::work::naming::expand_tilde;
use crate::work::tmux::{TmuxError, TmuxOps};

/// Default prompt template used when [`AuditConfig::prompt`] is unset. See
/// [`audit_prompt`].
const DEFAULT_PROMPT_TEMPLATE: &str = "/ticket-audit {key}";

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

/// Errors returned by [`launch_audit`].
#[derive(Debug, Error)]
pub enum AuditLaunchError {
    /// `[work.audit].dir` is unset, so there is nowhere to launch the
    /// session. Per `docs/plans/board-audits.md`'s "Launch" design, this is
    /// a status-line error for the caller to surface, not a crash.
    #[error("audit launching is not configured; set [work.audit].dir")]
    NotConfigured,

    /// A tmux session for this ticket is already live; launching again
    /// would double-run the audit. The caller should attach to
    /// `session_name` instead.
    #[error("an audit session is already running: {session_name}")]
    AlreadyRunning {
        /// Name of the already-live tmux session.
        session_name: String,
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
}

/// The deterministic tmux session name for `key`'s audit:
/// `tm-audit-<lowercased key>`. Deterministic so the board can map a live
/// tmux session back to a ticket, and attach, by name alone — no lookup
/// through the run store required.
pub fn audit_session_name(key: &str) -> String {
    format!("tm-audit-{}", key.to_lowercase())
}

/// Substitutes `{key}` in `template` (or [`DEFAULT_PROMPT_TEMPLATE`] when
/// `template` is `None`) with `key`, producing the prompt text handed to
/// `claude` on launch.
pub fn audit_prompt(template: Option<&str>, key: &str) -> String {
    template
        .unwrap_or(DEFAULT_PROMPT_TEMPLATE)
        .replace("{key}", key)
}

/// Quotes `s` as a single POSIX shell word: wraps it in single quotes,
/// escaping any embedded single quote as `'\''`. Needed because
/// [`TmuxOps::new_session_with_command`]'s `command` argument is a single
/// string tmux hands to the user's `$SHELL -c` — unlike the rest of this
/// codebase's `Command`/argv-based shelling-out (which never touches a
/// shell's string-splicing rules at all), this one positional string must
/// itself be valid shell syntax.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Builds the shell command line a tmux-hosted session runs: `claude` with an
/// optional `--model` and `prompt` as its positional argument, every value
/// [`shell_quote`]d.
///
/// `--model` is emitted only when `model` is `Some`, so an unconfigured launch
/// keeps the exact command shape it had before the option existed. Passing the
/// flag explicitly is what lets a launched session escape an
/// enterprise-managed model pin — see [`crate::config::RawAuditConfig::model`].
///
/// Shared with [`crate::work::bugbot`]'s cleanup launcher, which hosts its
/// sessions the same way.
pub(crate) fn claude_command(model: Option<&str>, prompt: &str) -> String {
    match model {
        Some(model) => format!(
            "claude --model {} {}",
            shell_quote(model),
            shell_quote(prompt)
        ),
        None => format!("claude {}", shell_quote(prompt)),
    }
}

/// Launches (or refuses to double-launch) a ticket-audit session for `key`.
///
/// 1. Errors with [`AuditLaunchError::NotConfigured`] if `audit_cfg.dir` is
///    unset.
/// 2. Errors with [`AuditLaunchError::AlreadyRunning`] if a tmux session
///    named [`audit_session_name`] already exists — no run row is created in
///    this case, so a launch attempt against an already-live session never
///    creates an orphaned pre-registration.
/// 3. Otherwise pre-registers a run (`kind = "audit"`, `lane = "audit"`,
///    `pid = None`; see the module docs) and starts the tmux session running
///    `claude <prompt>` (with `--model` when `audit_cfg.model` is set — see
///    [`claude_command`]), and `SESSION_RUN_ID_ENV` set to the new run's id so
///    the in-session `tm ticket audit` can adopt it.
///
/// `home` resolves a leading `~` in `audit_cfg.dir` via
/// [`crate::work::naming::expand_tilde`], matching every other `~`-expanding
/// config caller in this codebase (config values are never expanded by the
/// `config` module itself).
pub fn launch_audit(
    store: &RunStore,
    tmux: &dyn TmuxOps,
    audit_cfg: &AuditConfig,
    home: &Path,
    key: &str,
) -> Result<LaunchOutcome, AuditLaunchError> {
    let raw_dir = audit_cfg
        .dir
        .as_deref()
        .ok_or(AuditLaunchError::NotConfigured)?;
    let dir = expand_tilde(raw_dir, home);
    let dir_str = dir.to_string_lossy().into_owned();

    let session_name = audit_session_name(key);
    if tmux.has_session(&session_name)? {
        return Err(AuditLaunchError::AlreadyRunning { session_name });
    }

    let run_id = store.start_run(&StartRun {
        ticket: key.to_string(),
        lane: "audit".to_string(),
        worktree: dir_str.clone(),
        branch: None,
        pid: None,
        kind: "audit".to_string(),
        log_path: None,
    })?;

    let prompt = audit_prompt(audit_cfg.prompt.as_deref(), key);
    let command = claude_command(audit_cfg.model.as_deref(), &prompt);
    let env = [(SESSION_RUN_ID_ENV.to_string(), run_id.to_string())];

    tmux.new_session_with_command(&session_name, &dir_str, AUDIT_WINDOW_NAME, &env, &command)?;

    Ok(LaunchOutcome {
        run_id,
        session_name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runs::RunStatus;
    use crate::work::tmux::{FakeTmuxOps, TmuxCall};
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

    /// The `command` string of the single `NewSessionWithCommand` call `tmux`
    /// recorded, for the tests that only care about how `claude` was invoked.
    fn launched_command(tmux: &FakeTmuxOps) -> Option<String> {
        tmux.calls().iter().find_map(|call| match call {
            TmuxCall::NewSessionWithCommand { command, .. } => Some(command.clone()),
            _ => None,
        })
    }

    #[test]
    fn audit_session_name_lowercases_the_key() {
        assert_eq!(audit_session_name("PROJ-123"), "tm-audit-proj-123");
    }

    #[test]
    fn audit_prompt_defaults_to_ticket_audit_template() {
        assert_eq!(audit_prompt(None, "PROJ-1"), "/ticket-audit PROJ-1");
    }

    #[test]
    fn audit_prompt_substitutes_key_in_custom_template() {
        assert_eq!(
            audit_prompt(Some("/custom-audit for {key} please"), "PROJ-1"),
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

        let outcome = launch_audit(&store, &tmux, &audit_cfg, &home, "PROJ-1")
            .expect("launch should succeed");

        assert_eq!(outcome.session_name, "tm-audit-proj-1");

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

        assert_eq!(
            tmux.calls(),
            vec![
                TmuxCall::HasSession("tm-audit-proj-1".to_string()),
                TmuxCall::NewSessionWithCommand {
                    name: "tm-audit-proj-1".to_string(),
                    dir: "/Users/jowi/Projects/axiom".to_string(),
                    window_name: "audit".to_string(),
                    env: vec![(SESSION_RUN_ID_ENV.to_string(), outcome.run_id.to_string())],
                    command: "claude '/ticket-audit PROJ-1'".to_string(),
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

        let err = launch_audit(&store, &tmux, &audit_cfg, &home, "PROJ-1")
            .expect_err("should refuse to launch");

        assert!(matches!(err, AuditLaunchError::NotConfigured));
        assert!(store.list_runs().unwrap().is_empty());
        assert!(tmux.calls().is_empty());
    }

    #[test]
    fn launch_audit_errors_and_creates_no_run_when_already_running() {
        let db_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());
        let tmux = FakeTmuxOps::new().with_has_session(Ok(true));
        let home = PathBuf::from("/Users/jowi");
        let audit_cfg = configured("~/Projects/axiom");

        let err = launch_audit(&store, &tmux, &audit_cfg, &home, "PROJ-1")
            .expect_err("should refuse to double-launch");

        match err {
            AuditLaunchError::AlreadyRunning { session_name } => {
                assert_eq!(session_name, "tm-audit-proj-1");
            }
            other => panic!("expected AlreadyRunning, got {other:?}"),
        }
        assert!(
            store.list_runs().unwrap().is_empty(),
            "must not pre-register a run for a session that already exists"
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

        launch_audit(&store, &tmux, &audit_cfg, &home, "PROJ-9").unwrap();

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

        launch_audit(&store, &tmux, &audit_cfg, &home, "PROJ-9").unwrap();

        assert_eq!(
            launched_command(&tmux),
            Some("claude --model 'opus' '/ticket-audit PROJ-9'".to_string())
        );
    }
}
