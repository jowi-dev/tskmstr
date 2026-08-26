//! Board-launched bugbot-cleanup sessions: `docs/plans/bugbot-watch.md`'s
//! "Cleanup session: recommendation" and "Findings-to-prompt plumbing"
//! sections.
//!
//! A cleanup session is an *interactive* `claude` conversation, structurally
//! identical to [`crate::work::audit::launch_audit`]'s launch sequence (same
//! refuse-if-live / pre-register / `tmux.new_session_with_command` shape),
//! not a headless `tm work run` lane — findings need human judgment, per the
//! plan's rationale. [`launch_cleanup`] is the whole launch sequence; the
//! only shape difference from `launch_audit` is a second prompt placeholder,
//! `{findings_file}`, pointing at the file
//! [`crate::work::review_watch::poll_once`] already wrote before finishing
//! the run as [`crate::runs::RunStatus::Review`] — this module only needs
//! that file's *path*, never writes it itself.
//!
//! [`RealCleanupLauncher`] adapts [`launch_cleanup`] to
//! [`crate::work::review_watch::CleanupLauncher`], the seam
//! `tm pr watch`'s poll loop calls when `on_bots_done == Launch`: per that
//! trait's contract, a launch failure here is reported (`eprintln`) but
//! never propagated, since the run is already finished by the time the
//! launcher is invoked.

use std::path::Path;

use thiserror::Error;

use crate::config::{BackendIdentity, ReviewWatchConfig};
use crate::runs::{RunStore, RunStoreError, StartRun};
use crate::work::audit::SHELL_WINDOW_NAME;
use crate::work::naming::{expand_tilde, ticket_session_name};
use crate::work::review_watch::{CleanupLauncher, findings_file_path};
use crate::work::tmux::{
    TmuxError, TmuxOps, has_live_window, session_window_names, unique_window_name,
};

/// Default prompt template used when [`ReviewWatchConfig::prompt`] is unset.
/// See [`cleanup_prompt`].
const DEFAULT_PROMPT_TEMPLATE: &str = "/bugbot-triage {key} {findings_file}";

/// Name of the tmux window a launched cleanup session's `claude` process
/// runs in. Public for the same reason
/// [`crate::work::audit::AUDIT_WINDOW_NAME`] is: it is the board's liveness
/// signal for cleanup sessions.
///
/// Named for the action (`bugbot`), not for the run `kind`
/// (`bugbot-cleanup`): it sits alongside `audit`, `work` and `fix` in the
/// ticket's session, where the shorter name reads as the action history it
/// is.
pub const CLEANUP_WINDOW_NAME: &str = "bugbot";

/// `kind`/`lane` value [`launch_cleanup`] stores for the pre-registered run
/// row.
const CLEANUP_KIND: &str = "bugbot-cleanup";

/// Errors returned by [`launch_cleanup`].
#[derive(Debug, Error)]
pub enum CleanupLaunchError {
    /// Neither `[work.review_watch].dir` nor its `[work.audit].dir` fallback
    /// (already applied by [`crate::config::merge_work`] by the time a
    /// [`ReviewWatchConfig`] reaches this function) is set, so there is
    /// nowhere to launch the session.
    #[error(
        "bugbot-cleanup launching is not configured; set [work.review_watch].dir or [work.audit].dir"
    )]
    NotConfigured,

    /// This ticket's cleanup window is already live; launching again would
    /// double-run the cleanup session. The caller should attach to
    /// `session_name` instead. Window-scoped for the same reason
    /// [`crate::work::audit::AuditLaunchError::AlreadyRunning`] is.
    #[error("a bugbot-cleanup session is already running: {session_name}:{window_name}")]
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

/// Successful outcome of [`launch_cleanup`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchOutcome {
    /// Id of the pre-registered run row (`kind = "bugbot-cleanup"`).
    pub run_id: i64,
    /// Name of the tmux session the caller can attach to.
    pub session_name: String,
    /// Name of the window `claude` runs in: [`CLEANUP_WINDOW_NAME`], or a
    /// suffixed variant when a dead predecessor still holds that name (see
    /// [`crate::work::tmux::unique_window_name`]).
    pub window_name: String,
}

/// Substitutes `{key}` and `{findings_file}` in `template` (or
/// [`DEFAULT_PROMPT_TEMPLATE`] when `template` is `None`), producing the
/// prompt text handed to `claude` on launch. Mirrors
/// [`crate::work::audit::audit_prompt`], with a second placeholder.
pub fn cleanup_prompt(template: Option<&str>, key: &str, findings_file: &Path) -> String {
    template
        .unwrap_or(DEFAULT_PROMPT_TEMPLATE)
        .replace("{key}", key)
        .replace("{findings_file}", &findings_file.to_string_lossy())
}

/// Dependencies [`launch_cleanup`] writes to, gathered so callers don't have
/// to thread separate parameters through, mirroring
/// [`crate::work::review_watch::PollDeps`]'s shape.
pub struct CleanupLaunchDeps<'a> {
    /// The run-state store the pre-registered row is written to.
    pub store: &'a RunStore,
    /// Tmux operations (real or fake).
    pub tmux: &'a dyn TmuxOps,
}

/// Everything [`launch_cleanup`] needs to know about *this* launch, mirroring
/// [`crate::work::review_watch::PollRequest`]'s split from
/// [`CleanupLaunchDeps`].
pub struct CleanupLaunchRequest<'a> {
    /// Validated `[work.review_watch]` config (`dir` already carries the
    /// `[work.audit].dir` fallback applied by
    /// [`crate::config::merge_work`] — see [`CleanupLaunchError::NotConfigured`]).
    pub cfg: &'a ReviewWatchConfig,
    /// The invoking user's home directory, for `~`-expanding `cfg.dir` and
    /// for the findings-file path fallback.
    pub home: &'a Path,
    /// `$XDG_DATA_HOME`, if set, for the findings-file path.
    pub xdg_data_home: Option<&'a Path>,
    /// The invoking repo's [`BackendIdentity`]; its
    /// [`session_slug`](BackendIdentity::session_slug) qualifies the ticket's
    /// session name so same-numbered tickets in different repos never share
    /// a session (GitHub issue #10).
    pub identity: &'a BackendIdentity,
    /// The ticket key this cleanup session is for, e.g. `PROJ-372`.
    pub key: &'a str,
}

/// Launches (or refuses to double-launch) a bugbot-cleanup session for
/// `req.key`, following [`crate::work::audit::launch_audit`]'s sequence
/// near-verbatim:
///
/// 1. Errors with [`CleanupLaunchError::NotConfigured`] if `req.cfg.dir` is
///    unset (already fallback-resolved against `[work.audit].dir` by the
///    time it reaches here — this function does not re-apply that
///    fallback).
/// 2. Errors with [`CleanupLaunchError::AlreadyRunning`] if a live window
///    named [`CLEANUP_WINDOW_NAME`] already exists in
///    the ticket's [`crate::work::naming::ticket_session_name`] session — no run row is created in this
///    case.
/// 3. Otherwise pre-registers a run (`kind = "bugbot-cleanup"`, `lane =
///    "bugbot-cleanup"`, `pid = None`) and starts the tmux session running
///    `claude <prompt>`, with [`crate::work::audit::SESSION_RUN_ID_ENV`] set
///    to the new run's id so the in-session `/bugbot-triage` skill's `tm
///    runs register --kind bugbot-cleanup` step can adopt it.
///
/// The prompt's `{findings_file}` placeholder is filled from
/// [`findings_file_path`] — the findings file itself already exists by the
/// time a `tm pr watch` tick calls this (it wrote the file before finishing
/// the run as `Review`), so this function only needs the path, never writes
/// it.
pub fn launch_cleanup(
    deps: &CleanupLaunchDeps<'_>,
    req: &CleanupLaunchRequest<'_>,
) -> Result<LaunchOutcome, CleanupLaunchError> {
    let raw_dir = req
        .cfg
        .dir
        .as_deref()
        .ok_or(CleanupLaunchError::NotConfigured)?;
    let dir = expand_tilde(raw_dir, req.home);
    let dir_str = dir.to_string_lossy().into_owned();

    let session_name = ticket_session_name(&req.identity.session_slug(), req.key);
    // One snapshot answers both "already running?" and "does the session
    // exist yet?"; see `launch_audit`.
    let windows = deps.tmux.list_windows()?;
    if has_live_window(&windows, &session_name, CLEANUP_WINDOW_NAME) {
        return Err(CleanupLaunchError::AlreadyRunning {
            session_name,
            window_name: CLEANUP_WINDOW_NAME.to_string(),
        });
    }
    let existing_windows = session_window_names(&windows, &session_name);
    let window_name = unique_window_name(CLEANUP_WINDOW_NAME, &existing_windows);

    let run_id = deps.store.start_run(&StartRun {
        ticket: req.key.to_string(),
        lane: CLEANUP_KIND.to_string(),
        worktree: dir_str.clone(),
        branch: None,
        pid: None,
        kind: CLEANUP_KIND.to_string(),
        log_path: None,
    })?;

    let findings_file = findings_file_path(req.home, req.xdg_data_home, req.key);
    let prompt = cleanup_prompt(req.cfg.prompt.as_deref(), req.key, &findings_file);
    let command = crate::work::audit::claude_command(req.cfg.model.as_deref(), &prompt);
    let env = [(
        crate::work::audit::SESSION_RUN_ID_ENV.to_string(),
        run_id.to_string(),
    )];

    if existing_windows.is_empty() {
        deps.tmux.new_session_with_command(
            &session_name,
            &dir_str,
            &window_name,
            &env,
            &command,
        )?;
        // Creating the ticket's session, so provision its shell window too
        // and hand focus back to the action window, exactly as
        // `launch_audit` does.
        deps.tmux
            .new_window(&session_name, SHELL_WINDOW_NAME, &dir_str)?;
        deps.tmux.select_window(&session_name, &window_name)?;
    } else {
        deps.tmux
            .new_window_with_command(&session_name, &window_name, &dir_str, &env, &command)?;
    }

    Ok(LaunchOutcome {
        run_id,
        session_name,
        window_name,
    })
}

/// Adapts [`launch_cleanup`] to [`CleanupLauncher`], the seam `tm pr
/// watch`'s poll loop calls when `on_bots_done == Launch`. Owns references to
/// everything [`launch_cleanup`] needs so `src/main.rs` can build one and
/// hand it to [`crate::cli::pr::PrWatchDeps::cleanup_launcher`].
///
/// Per [`CleanupLauncher`]'s contract, a launch failure is reported
/// (`eprintln`) but never propagated: the run this launcher is invoked for
/// has already been finished as [`crate::runs::RunStatus::Review`] by the
/// time [`CleanupLauncher::launch_cleanup`] runs, so a launch failure here
/// must not un-finish it or block the poll loop from exiting cleanly.
pub struct RealCleanupLauncher<'a> {
    /// The run-state store the pre-registered row is written to.
    pub store: &'a RunStore,
    /// Tmux operations (real or fake).
    pub tmux: &'a dyn TmuxOps,
    /// Validated `[work.review_watch]` config.
    pub cfg: &'a ReviewWatchConfig,
    /// The invoking user's home directory.
    pub home: &'a Path,
    /// `$XDG_DATA_HOME`, if set.
    pub xdg_data_home: Option<&'a Path>,
    /// The invoking repo's [`BackendIdentity`] (see
    /// [`CleanupLaunchRequest::identity`]).
    pub identity: &'a BackendIdentity,
}

impl CleanupLauncher for RealCleanupLauncher<'_> {
    fn launch_cleanup(&self, key: &str) {
        let deps = CleanupLaunchDeps {
            store: self.store,
            tmux: self.tmux,
        };
        let req = CleanupLaunchRequest {
            cfg: self.cfg,
            home: self.home,
            xdg_data_home: self.xdg_data_home,
            identity: self.identity,
            key,
        };
        if let Err(err) = launch_cleanup(&deps, &req) {
            eprintln!("warning: failed to launch bugbot-cleanup session for {key}: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runs::RunStatus;
    use crate::work::review_watch::findings_file_path;
    use crate::work::tmux::{FakeTmuxOps, TmuxCall, TmuxWindow};
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn open_store(dir: &Path) -> RunStore {
        RunStore::open(&dir.join("runs.db")).expect("open should succeed")
    }

    fn configured(dir: &str) -> ReviewWatchConfig {
        ReviewWatchConfig {
            dir: Some(dir.to_string()),
            ..ReviewWatchConfig::default()
        }
    }

    /// Canonical test identity; its `session_slug()` is `proj`, so ticket
    /// sessions in these tests are named `tm-proj-<lowercased key>`.
    fn test_identity() -> &'static crate::config::BackendIdentity {
        static IDENTITY: std::sync::OnceLock<crate::config::BackendIdentity> =
            std::sync::OnceLock::new();
        IDENTITY.get_or_init(|| crate::config::BackendIdentity::Jira {
            base_url: "https://x.atlassian.net".to_string(),
            project_key: "PROJ".to_string(),
        })
    }

    #[test]
    fn cleanup_prompt_defaults_to_bugbot_triage_template() {
        let findings_file = PathBuf::from("/home/user/.local/share/tskmstr/findings/proj-1.json");
        assert_eq!(
            cleanup_prompt(None, "PROJ-1", &findings_file),
            "/bugbot-triage PROJ-1 /home/user/.local/share/tskmstr/findings/proj-1.json"
        );
    }

    #[test]
    fn cleanup_prompt_substitutes_both_placeholders_in_custom_template() {
        let findings_file = PathBuf::from("/tmp/findings.json");
        assert_eq!(
            cleanup_prompt(
                Some("/custom {key} using {findings_file} please"),
                "PROJ-1",
                &findings_file
            ),
            "/custom PROJ-1 using /tmp/findings.json please"
        );
    }

    #[test]
    fn launch_cleanup_creates_run_and_starts_tmux_session() {
        let db_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());
        let tmux = FakeTmuxOps::new();
        let home = PathBuf::from("/Users/jowi");
        let cfg = configured("~/Projects/axiom");
        let deps = CleanupLaunchDeps {
            store: &store,
            tmux: &tmux,
        };
        let req = CleanupLaunchRequest {
            cfg: &cfg,
            home: &home,
            xdg_data_home: None,
            identity: test_identity(),
            key: "PROJ-1",
        };

        let outcome = launch_cleanup(&deps, &req).expect("launch should succeed");

        assert_eq!(outcome.session_name, "tm-proj-proj-1");

        let run = store
            .run_by_id(outcome.run_id)
            .unwrap()
            .expect("run row should exist");
        assert_eq!(run.ticket, "PROJ-1");
        assert_eq!(run.kind, "bugbot-cleanup");
        assert_eq!(run.lane, "bugbot-cleanup");
        assert_eq!(run.status, RunStatus::Running);
        assert_eq!(run.pid, None);
        assert_eq!(run.worktree, "/Users/jowi/Projects/axiom");

        assert_eq!(outcome.window_name, CLEANUP_WINDOW_NAME);
        let findings_file = findings_file_path(&home, None, "PROJ-1");
        assert_eq!(
            tmux.calls(),
            vec![
                TmuxCall::ListWindows,
                TmuxCall::NewSessionWithCommand {
                    name: "tm-proj-proj-1".to_string(),
                    dir: "/Users/jowi/Projects/axiom".to_string(),
                    window_name: CLEANUP_WINDOW_NAME.to_string(),
                    env: vec![(
                        crate::work::audit::SESSION_RUN_ID_ENV.to_string(),
                        outcome.run_id.to_string()
                    )],
                    command: format!(
                        "claude '/bugbot-triage PROJ-1 {}'",
                        findings_file.to_string_lossy()
                    ),
                },
                TmuxCall::NewWindow {
                    name: "tm-proj-proj-1".to_string(),
                    window_name: SHELL_WINDOW_NAME.to_string(),
                    dir: "/Users/jowi/Projects/axiom".to_string(),
                },
                TmuxCall::SelectWindow {
                    name: "tm-proj-proj-1".to_string(),
                    window: CLEANUP_WINDOW_NAME.to_string(),
                },
            ]
        );
    }

    #[test]
    fn launch_cleanup_errors_when_dir_is_unset() {
        let db_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());
        let tmux = FakeTmuxOps::new();
        let home = PathBuf::from("/Users/jowi");
        let cfg = ReviewWatchConfig::default();
        let deps = CleanupLaunchDeps {
            store: &store,
            tmux: &tmux,
        };
        let req = CleanupLaunchRequest {
            cfg: &cfg,
            home: &home,
            xdg_data_home: None,
            identity: test_identity(),
            key: "PROJ-1",
        };

        let err = launch_cleanup(&deps, &req).expect_err("should refuse to launch");

        assert!(matches!(err, CleanupLaunchError::NotConfigured));
        assert!(store.list_runs().unwrap().is_empty());
        assert!(tmux.calls().is_empty());
    }

    #[test]
    fn launch_cleanup_errors_and_creates_no_run_when_the_cleanup_window_is_live() {
        let db_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());
        let tmux = FakeTmuxOps::new().with_list_windows(Ok(vec![TmuxWindow {
            session: "tm-proj-proj-1".to_string(),
            name: CLEANUP_WINDOW_NAME.to_string(),
            dead: false,
        }]));
        let home = PathBuf::from("/Users/jowi");
        let cfg = configured("~/Projects/axiom");
        let deps = CleanupLaunchDeps {
            store: &store,
            tmux: &tmux,
        };
        let req = CleanupLaunchRequest {
            cfg: &cfg,
            home: &home,
            xdg_data_home: None,
            identity: test_identity(),
            key: "PROJ-1",
        };

        let err = launch_cleanup(&deps, &req).expect_err("should refuse to double-launch");

        match err {
            CleanupLaunchError::AlreadyRunning {
                session_name,
                window_name,
            } => {
                assert_eq!(session_name, "tm-proj-proj-1");
                assert_eq!(window_name, CLEANUP_WINDOW_NAME);
            }
            other => panic!("expected AlreadyRunning, got {other:?}"),
        }
        assert!(
            store.list_runs().unwrap().is_empty(),
            "must not pre-register a run for an action that is already running"
        );
    }

    #[test]
    fn launch_cleanup_appends_a_window_when_the_ticket_session_already_exists() {
        let db_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());
        let tmux = FakeTmuxOps::new().with_list_windows(Ok(vec![TmuxWindow {
            session: "tm-proj-proj-1".to_string(),
            name: "audit".to_string(),
            dead: false,
        }]));
        let home = PathBuf::from("/Users/jowi");
        let cfg = configured("/repo/axiom");
        let deps = CleanupLaunchDeps {
            store: &store,
            tmux: &tmux,
        };
        let req = CleanupLaunchRequest {
            cfg: &cfg,
            home: &home,
            xdg_data_home: None,
            identity: test_identity(),
            key: "PROJ-1",
        };

        let outcome = launch_cleanup(&deps, &req).expect("launch should succeed");

        let findings_file = findings_file_path(&home, None, "PROJ-1");
        assert_eq!(
            tmux.calls(),
            vec![
                TmuxCall::ListWindows,
                TmuxCall::NewWindowWithCommand {
                    name: "tm-proj-proj-1".to_string(),
                    window_name: CLEANUP_WINDOW_NAME.to_string(),
                    dir: "/repo/axiom".to_string(),
                    env: vec![(
                        crate::work::audit::SESSION_RUN_ID_ENV.to_string(),
                        outcome.run_id.to_string()
                    )],
                    command: format!(
                        "claude '/bugbot-triage PROJ-1 {}'",
                        findings_file.to_string_lossy()
                    ),
                },
            ]
        );
    }

    #[test]
    fn launch_cleanup_uses_custom_prompt_template() {
        let db_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());
        let tmux = FakeTmuxOps::new();
        let home = PathBuf::from("/Users/jowi");
        let cfg = ReviewWatchConfig {
            dir: Some("/repo/axiom".to_string()),
            prompt: Some("/custom-triage {key} reading {findings_file}".to_string()),
            ..ReviewWatchConfig::default()
        };
        let deps = CleanupLaunchDeps {
            store: &store,
            tmux: &tmux,
        };
        let req = CleanupLaunchRequest {
            cfg: &cfg,
            home: &home,
            xdg_data_home: None,
            identity: test_identity(),
            key: "PROJ-9",
        };

        launch_cleanup(&deps, &req).unwrap();

        let findings_file = findings_file_path(&home, None, "PROJ-9");
        let calls = tmux.calls();
        let command = calls.iter().find_map(|call| match call {
            TmuxCall::NewSessionWithCommand { command, .. }
            | TmuxCall::NewWindowWithCommand { command, .. } => Some(command.clone()),
            _ => None,
        });
        assert_eq!(
            command,
            Some(format!(
                "claude '/custom-triage PROJ-9 reading {}'",
                findings_file.to_string_lossy()
            ))
        );
    }

    #[test]
    fn launch_cleanup_passes_configured_model_to_claude() {
        let db_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());
        let tmux = FakeTmuxOps::new();
        let home = PathBuf::from("/Users/jowi");
        let cfg = ReviewWatchConfig {
            dir: Some("/repo/axiom".to_string()),
            model: Some("fable".to_string()),
            ..ReviewWatchConfig::default()
        };
        let deps = CleanupLaunchDeps {
            store: &store,
            tmux: &tmux,
        };
        let req = CleanupLaunchRequest {
            cfg: &cfg,
            home: &home,
            xdg_data_home: None,
            identity: test_identity(),
            key: "PROJ-9",
        };

        launch_cleanup(&deps, &req).unwrap();

        let findings_file = findings_file_path(&home, None, "PROJ-9");
        let calls = tmux.calls();
        let command = calls.iter().find_map(|call| match call {
            TmuxCall::NewSessionWithCommand { command, .. }
            | TmuxCall::NewWindowWithCommand { command, .. } => Some(command.clone()),
            _ => None,
        });
        assert_eq!(
            command,
            Some(format!(
                "claude --model 'fable' '/bugbot-triage PROJ-9 {}'",
                findings_file.to_string_lossy()
            ))
        );
    }

    #[test]
    fn real_cleanup_launcher_launches_session_via_launch_cleanup() {
        let db_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());
        let tmux = FakeTmuxOps::new();
        let home = PathBuf::from("/Users/jowi");
        let cfg = configured("~/Projects/axiom");
        let launcher = RealCleanupLauncher {
            store: &store,
            tmux: &tmux,
            cfg: &cfg,
            home: &home,
            xdg_data_home: None,
            identity: test_identity(),
        };

        launcher.launch_cleanup("PROJ-1");

        assert_eq!(
            tmux.calls().len(),
            4,
            "should have snapshotted the windows, then created the session, its shell window, and reselected the action window"
        );
        let run = store
            .list_runs()
            .unwrap()
            .into_iter()
            .find(|r| r.ticket == "PROJ-1")
            .expect("run row should exist");
        assert_eq!(run.kind, "bugbot-cleanup");
    }

    #[test]
    fn real_cleanup_launcher_does_not_panic_when_not_configured() {
        let db_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());
        let tmux = FakeTmuxOps::new();
        let home = PathBuf::from("/Users/jowi");
        let cfg = ReviewWatchConfig::default();
        let launcher = RealCleanupLauncher {
            store: &store,
            tmux: &tmux,
            cfg: &cfg,
            home: &home,
            xdg_data_home: None,
            identity: test_identity(),
        };

        // Must not propagate/panic per the `CleanupLauncher` contract.
        launcher.launch_cleanup("PROJ-1");

        assert!(store.list_runs().unwrap().is_empty());
    }
}
