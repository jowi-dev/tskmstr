//! Board-launched ticket-audit sessions: `docs/plans/board-audits.md`'s
//! "Launch" design.
//!
//! An audit session is an *interactive* `claude` conversation, unlike a
//! headless `tm work run` lane, so it is tmux-hosted: started detached via
//! [`TmuxOps::new_session_with_command`], attached on demand (the board's
//! job, not this module's). [`launch_audit`] is the whole launch sequence:
//! refuse to double-launch, pre-register a run row, then start the tmux
//! session running `claude` with the audit prompt. The session is rooted in
//! the ticket's lane worktree when one can be resolved, so the ticket's tmux
//! session reports the ticket's branch (GitHub issue #252); see
//! [`resolve_audit_placement`].
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
use crate::config::{AuditConfig, BackendIdentity, LaneConfig};
use crate::runs::{RunStore, RunStoreError, StartRun};
use crate::work::git::GitOps;
use crate::work::naming::{expand_tilde, ticket_session_name};
use crate::work::run::{LaneWorktree, RunLaneError, lane_base, lane_worktree};
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

    /// `audit_cfg.prompt_file` was set but could not be read. Checked before
    /// [`RunStore::start_run`], so a bad path leaves no orphaned run row.
    #[error(transparent)]
    PromptFile(#[from] crate::work::prompt::PromptFileError),

    /// Memory admission refused the session (GitHub issue #149).
    #[error(transparent)]
    Admission(#[from] crate::work::admission::AdmissionError),

    /// The ticket's lane worktree could not be provisioned (GitHub issue
    /// #252). Raised before [`RunStore::start_run`], so no run row is left
    /// behind.
    #[error("failed to provision the ticket worktree {}: {source}", path.display())]
    Provision {
        /// The worktree path that was being provisioned.
        path: std::path::PathBuf,
        /// What went wrong: resolving the base, or `git worktree add`.
        #[source]
        source: Box<RunLaneError>,
    },
}

/// What [`launch_audit`] needs to root an audit in the ticket's lane
/// worktree instead of `[work.audit].dir` (GitHub issue #252).
pub struct AuditWorktrees<'a> {
    /// Provisions the worktree when it is not on disk yet.
    pub git: &'a dyn GitOps,
    /// `[work] worktree_root`, already `~`-expanded (see
    /// [`crate::work::run::resolve_worktree_root`]).
    pub worktree_root: std::path::PathBuf,
    /// The lanes compatible with the invoking repo's backend (see
    /// [`crate::config::compatible_lane_names`]), in config order.
    pub lanes: Vec<(String, LaneConfig)>,
}

/// Where an audit for one ticket runs: see [`resolve_audit_placement`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditPlacement {
    /// The ticket's lane worktree, the same one `tm work run <lane> <key>`
    /// uses.
    Worktree {
        /// The lane whose worktree this is.
        lane: String,
        /// The lane's worktree for the ticket.
        worktree: LaneWorktree,
    },
    /// `[work.audit].dir`, because no single lane worktree could be
    /// resolved; `reason` says why, for the launch status.
    Checkout {
        /// Why the audit is not running in a ticket worktree.
        reason: String,
    },
}

/// Picks where `key`'s audit runs. The lane worktree is used when every lane
/// in `lanes` puts the ticket in the same worktree (one lane, or several
/// sharing a repo); the first lane then supplies the base. With no lanes,
/// or lanes that disagree, there is no single worktree a later lane run is
/// sure to reuse, so the audit falls back to the checkout and says why.
pub fn resolve_audit_placement(
    worktree_root: &Path,
    lanes: &[(String, LaneConfig)],
    key: &str,
) -> AuditPlacement {
    let Some((first_lane, first_config)) = lanes.first() else {
        return AuditPlacement::Checkout {
            reason: "no lane is configured for this repo".to_string(),
        };
    };
    let worktree = match lane_worktree(worktree_root, first_lane, first_config, Some(key)) {
        Ok(worktree) => worktree,
        Err(err) => {
            return AuditPlacement::Checkout {
                reason: err.to_string(),
            };
        }
    };
    let disagreeing: Vec<&str> = lanes[1..]
        .iter()
        .filter(|(name, config)| {
            lane_worktree(worktree_root, name, config, Some(key))
                .map_or(true, |other| other.wt_path != worktree.wt_path)
        })
        .map(|(name, _)| name.as_str())
        .collect();
    if !disagreeing.is_empty() {
        return AuditPlacement::Checkout {
            reason: format!(
                "lanes {first_lane}, {} keep {key} in different worktrees",
                disagreeing.join(", ")
            ),
        };
    }
    AuditPlacement::Worktree {
        lane: first_lane.clone(),
        worktree,
    }
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
    /// Admission's warning for a launch that went ahead anyway (over a
    /// usage ceiling, or usage unknown), for the board's status line.
    pub warning: Option<String>,
    /// Why the audit runs in `[work.audit].dir` rather than the ticket's
    /// lane worktree, for the board's status line; `None` when it runs in
    /// the worktree (see [`resolve_audit_placement`]).
    pub fallback: Option<String>,
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
/// 3. Otherwise reads `audit_cfg.prompt_file` (if set — see
///    [`crate::work::prompt::read_prompt_file`]) *before* pre-registering a
///    run, so a bad path leaves no orphaned run row (same posture as
///    [`crate::work::run::prepare_run_lane`]'s prompt preflight), pre-
///    registers a run (`kind = "audit"`, `lane = "audit"`, `pid = None`,
///    `agent`/`repo` the runner's name and the lane's repo, or the audit directory's name via
///    [`crate::work::run::dir_repo_key`]; see the module docs) and starts `claude <prompt>` (with `--model` when
///    `audit_cfg.model` is set — see
///    [`AgentRunner::interactive_shell_command`]) in
///    that window, with `SESSION_RUN_ID_ENV` set to the new run's id so the
///    in-session `tm ticket audit` can adopt it. The session (plus its
///    [`SHELL_WINDOW_NAME`] window) is created if this is the ticket's first
///    tmux-hosted action, and the window appended to it otherwise; the window
///    takes a [`unique_window_name`] suffix if a dead predecessor still holds
///    the plain name.
///
/// **Where it runs (GitHub issue #252).** The session's directory, and the
/// run row's `worktree`, is the ticket's lane worktree when
/// [`resolve_audit_placement`] finds one in `worktrees`: provisioned after
/// admission and before the run row if it is not on disk yet (a local
/// branch only, nothing pushed), reused otherwise, so a later lane run for
/// the ticket finds it and skips provisioning. Otherwise the session runs in
/// `audit_cfg.dir` and [`LaunchOutcome::fallback`] says why. A failed
/// provisioning is [`AuditLaunchError::Provision`] with no run row.
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
///
/// The session's model is `audit_cfg.model`, else the rest of the chain
/// `model_defaults` ends (GitHub issue #143, [`crate::work::model`]), so
/// `--model` is always passed.
#[allow(clippy::too_many_arguments)]
pub fn launch_audit(
    admission: &crate::work::admission::AdmissionDeps<'_>,
    store: &RunStore,
    tmux: &dyn TmuxOps,
    worktrees: &AuditWorktrees<'_>,
    audit_cfg: &AuditConfig,
    home: &Path,
    identity: &BackendIdentity,
    runner: &dyn AgentRunner,
    model_defaults: crate::work::model::ModelDefaults<'_>,
    key: &str,
) -> Result<LaunchOutcome, AuditLaunchError> {
    let raw_dir = audit_cfg
        .dir
        .as_deref()
        .ok_or(AuditLaunchError::NotConfigured)?;
    let audit_dir = expand_tilde(raw_dir, home);

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

    let prompt_file_contents = audit_cfg
        .prompt_file
        .as_deref()
        .map(|raw| crate::work::prompt::read_prompt_file(raw, home, "[work.audit].prompt_file"))
        .transpose()?;

    let placement = resolve_audit_placement(&worktrees.worktree_root, &worktrees.lanes, key);
    let (dir, repo, fallback) = match &placement {
        AuditPlacement::Worktree { worktree, .. } => {
            (worktree.wt_path.clone(), worktree.repo.clone(), None)
        }
        AuditPlacement::Checkout { reason } => (
            audit_dir.clone(),
            crate::work::run::dir_repo_key(&audit_dir),
            Some(reason.clone()),
        ),
    };
    let dir_str = dir.to_string_lossy().into_owned();

    // Admission (GitHub issue #149) before the row, so a refusal leaves
    // none. A board audit is human-started: over the memory budget it
    // refuses, over a usage ceiling it warns and launches.
    let mut warning = Vec::new();
    let admitted = admission.admit(
        store,
        &crate::work::admission::LaunchRequest {
            kind: "audit",
            agent: runner.name(),
            repo: &repo,
            own_run: None,
        },
        crate::work::admission::LaunchOrigin::Human,
        &mut warning,
    )?;

    if let AuditPlacement::Worktree { lane, worktree } = &placement {
        provision_lane_worktree(worktrees, lane, worktree)?;
    }

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
    // Every agent row carries its estimate key (GitHub issue #156): the
    // lane's repo, or the audit directory's name standing in for one.
    store.update_agent_repo(run_id, runner.name(), &repo)?;

    let template = prompt_file_contents
        .as_deref()
        .or(audit_cfg.prompt.as_deref())
        .unwrap_or_else(|| runner.default_audit_prompt_template());
    let prompt = audit_prompt(template, key);
    let model = model_defaults.resolve(
        runner,
        &[crate::work::model::ModelCandidate {
            source: "[work.audit] model",
            model: audit_cfg.model.as_deref(),
        }],
    );
    let command = runner.interactive_shell_command(&admitted, &model.model, &prompt);
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
        warning: crate::work::admission::warning_text(&warning),
        fallback,
    })
}

/// Creates `worktree` the way `tm work run`'s step 4 does when it is not on
/// disk yet: from [`lane_base`], checking out the [`LaneWorktree::wt_name`]
/// branch locally. Nothing is pushed. An existing worktree is reused as-is,
/// and a lane run launched afterwards sees it exists and skips provisioning.
fn provision_lane_worktree(
    worktrees: &AuditWorktrees<'_>,
    lane: &str,
    worktree: &LaneWorktree,
) -> Result<(), AuditLaunchError> {
    if worktree.wt_path.exists() {
        return Ok(());
    }
    let provision = || -> Result<(), RunLaneError> {
        let lane_config = worktrees
            .lanes
            .iter()
            .find(|(name, _)| name == lane)
            .map(|(_, config)| config)
            .ok_or_else(|| RunLaneError::UnknownLane(lane.to_string()))?;
        let base = lane_base(worktrees.git, lane, lane_config)?;
        worktrees.git.provision_worktree(
            &worktree.repo_root,
            &worktree.wt_path,
            &worktree.wt_name,
            Some(&base),
        )?;
        Ok(())
    };
    provision().map_err(|source| AuditLaunchError::Provision {
        path: worktree.wt_path.clone(),
        source: Box::new(source),
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
            prompt_file: None,
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

    /// No compatible lanes: every audit falls back to `[work.audit].dir`.
    fn no_lanes() -> AuditWorktrees<'static> {
        AuditWorktrees {
            git: Box::leak(Box::new(crate::work::git::FakeGitOps::new())),
            worktree_root: PathBuf::from("/Users/jowi/Worktrees"),
            lanes: Vec::new(),
        }
    }

    fn lane(repo: &str) -> crate::config::LaneConfig {
        crate::config::LaneConfig {
            repo: repo.to_string(),
            prompt_file: None,
            base_branch: Some("main".to_string()),
            model: None,
            max_turns: None,
            permission_mode: None,
            subagent: None,
        }
    }

    fn launch_with_lanes(
        store: &RunStore,
        tmux: &FakeTmuxOps,
        worktrees: &AuditWorktrees<'_>,
    ) -> Result<LaunchOutcome, AuditLaunchError> {
        launch_audit(
            &crate::work::admission::AdmissionDeps::off(),
            store,
            tmux,
            worktrees,
            &configured("~/Projects/axiom"),
            Path::new("/Users/jowi"),
            &test_identity(),
            &ClaudeRunner,
            crate::work::model::ModelDefaults::none(),
            "PROJ-1",
        )
    }

    /// GitHub issue #252: an audit on a ticket with a resolvable lane
    /// provisions that lane's worktree and roots the whole ticket session
    /// there, so the session's directory reports the ticket's branch.
    #[test]
    fn launch_audit_provisions_the_lane_worktree_and_runs_there() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        let tmux = FakeTmuxOps::new();
        let git = crate::work::git::FakeGitOps::new();
        let root = tmp.path().join("Worktrees");
        let worktrees = AuditWorktrees {
            git: &git,
            worktree_root: root.clone(),
            lanes: vec![("axiom".to_string(), lane("/Users/jowi/Projects/axiom"))],
        };

        let outcome = launch_with_lanes(&store, &tmux, &worktrees).unwrap();

        let wt_path = root.join("axiom/proj-1");
        let wt = wt_path.to_string_lossy().into_owned();
        assert_eq!(
            git.provision_worktree_calls(),
            vec![crate::work::git::ProvisionWorktreeCall {
                repo_dir: PathBuf::from("/Users/jowi/Projects/axiom"),
                wt_path: wt_path.clone(),
                branch: "proj-1".to_string(),
                from_base: Some("main".to_string()),
            }]
        );
        assert_eq!(outcome.fallback, None);
        let run = store.run_by_id(outcome.run_id).unwrap().unwrap();
        assert_eq!(run.worktree, wt);
        assert_eq!(run.lane, "audit");
        assert_eq!(run.repo.as_deref(), Some("axiom"));
        let calls = tmux.calls();
        assert!(
            matches!(&calls[1], TmuxCall::NewSessionWithCommand { dir, .. } if *dir == wt),
            "{calls:?}"
        );
        assert!(
            matches!(&calls[2], TmuxCall::NewWindow { window_name, dir, .. }
                if window_name == SHELL_WINDOW_NAME && *dir == wt),
            "{calls:?}"
        );
    }

    /// A worktree already on disk (an earlier audit, or a lane run) is
    /// reused as-is, never provisioned twice.
    #[test]
    fn launch_audit_reuses_an_existing_lane_worktree() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        let tmux = FakeTmuxOps::new();
        let git = crate::work::git::FakeGitOps::new();
        let root = tmp.path().join("Worktrees");
        let wt_path = root.join("axiom/proj-1");
        std::fs::create_dir_all(&wt_path).unwrap();
        let worktrees = AuditWorktrees {
            git: &git,
            worktree_root: root,
            lanes: vec![("axiom".to_string(), lane("/Users/jowi/Projects/axiom"))],
        };

        let outcome = launch_with_lanes(&store, &tmux, &worktrees).unwrap();

        assert!(git.provision_worktree_calls().is_empty());
        let run = store.run_by_id(outcome.run_id).unwrap().unwrap();
        assert_eq!(run.worktree, wt_path.to_string_lossy());
    }

    #[test]
    fn launch_audit_without_a_lane_falls_back_to_the_audit_dir_and_says_why() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        let tmux = FakeTmuxOps::new();

        let outcome = launch_with_lanes(&store, &tmux, &no_lanes()).unwrap();

        let reason = outcome.fallback.expect("a fallback reason");
        assert!(reason.contains("no lane"), "{reason}");
        let run = store.run_by_id(outcome.run_id).unwrap().unwrap();
        assert_eq!(run.worktree, "/Users/jowi/Projects/axiom");
    }

    /// Two compatible lanes that would put the ticket in different
    /// worktrees leave no single answer, so the audit stays in the checkout
    /// rather than guessing which lane the operator will launch.
    #[test]
    fn launch_audit_falls_back_when_lanes_disagree_on_the_worktree() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        let tmux = FakeTmuxOps::new();
        let git = crate::work::git::FakeGitOps::new();
        let worktrees = AuditWorktrees {
            git: &git,
            worktree_root: tmp.path().join("Worktrees"),
            lanes: vec![
                ("axiom".to_string(), lane("/Users/jowi/Projects/axiom")),
                ("other".to_string(), lane("/Users/jowi/Projects/other")),
            ],
        };

        let outcome = launch_with_lanes(&store, &tmux, &worktrees).unwrap();

        let reason = outcome.fallback.expect("a fallback reason");
        assert!(
            reason.contains("axiom") && reason.contains("other"),
            "{reason}"
        );
        assert!(git.provision_worktree_calls().is_empty());
    }

    /// Lanes sharing a repo agree on the worktree, so the first one wins.
    #[test]
    fn launch_audit_provisions_when_every_lane_agrees_on_the_worktree() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        let tmux = FakeTmuxOps::new();
        let git = crate::work::git::FakeGitOps::new();
        let worktrees = AuditWorktrees {
            git: &git,
            worktree_root: tmp.path().join("Worktrees"),
            lanes: vec![
                ("axiom".to_string(), lane("/Users/jowi/Projects/axiom")),
                ("partner".to_string(), lane("/Users/jowi/Projects/axiom")),
            ],
        };

        let outcome = launch_with_lanes(&store, &tmux, &worktrees).unwrap();

        assert_eq!(outcome.fallback, None);
        assert_eq!(git.provision_worktree_calls().len(), 1);
    }

    #[test]
    fn launch_audit_errors_and_creates_no_run_when_provisioning_fails() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        let tmux = FakeTmuxOps::new();
        let git = crate::work::git::FakeGitOps::new().with_provision_worktree_result(Err(
            crate::work::git::GitError::Command {
                command: "git worktree add".to_string(),
                exit_code: Some(128),
                stderr: "fatal: boom".to_string(),
            },
        ));
        let worktrees = AuditWorktrees {
            git: &git,
            worktree_root: tmp.path().join("Worktrees"),
            lanes: vec![("axiom".to_string(), lane("/Users/jowi/Projects/axiom"))],
        };

        let err = launch_with_lanes(&store, &tmux, &worktrees).unwrap_err();

        assert!(matches!(err, AuditLaunchError::Provision { .. }), "{err:?}");
        assert!(err.to_string().contains("proj-1"), "{err}");
        assert!(store.list_runs().unwrap().is_empty());
        assert!(
            !tmux
                .calls()
                .iter()
                .any(|c| matches!(c, TmuxCall::NewSessionWithCommand { .. })),
        );
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
    fn launch_audit_over_the_memory_budget_is_refused_before_its_row() {
        let db_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());
        let tmux = FakeTmuxOps::new();
        let home = PathBuf::from("/Users/jowi");
        let audit_cfg = configured("~/Projects/axiom");

        let err = launch_audit(
            &crate::work::admission::AdmissionDeps::with_budget(1),
            &store,
            &tmux,
            &no_lanes(),
            &audit_cfg,
            &home,
            &test_identity(),
            &ClaudeRunner,
            crate::work::model::ModelDefaults::none(),
            "PROJ-1",
        )
        .unwrap_err();

        assert!(
            err.to_string()
                .starts_with("not launching: memory budget exceeded: axiom/claude audit runs"),
            "{err}"
        );
        assert!(store.list_runs().unwrap().is_empty());
        assert!(
            !tmux
                .calls()
                .iter()
                .any(|c| matches!(c, TmuxCall::NewSessionWithCommand { .. })),
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
            &crate::work::admission::AdmissionDeps::off(),
            &store,
            &tmux,
            &no_lanes(),
            &audit_cfg,
            &home,
            &test_identity(),
            &ClaudeRunner,
            crate::work::model::ModelDefaults::none(),
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
        // GitHub issue #156: the audit row carries its estimate key.
        assert_eq!(run.agent.as_deref(), Some("claude"));
        assert_eq!(run.repo.as_deref(), Some("axiom"));

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
                    command: "claude --model 'opus' '/ticket-audit PROJ-1'".to_string(),
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
            &crate::work::admission::AdmissionDeps::off(),
            &store,
            &tmux,
            &no_lanes(),
            &audit_cfg,
            &home,
            &test_identity(),
            &ClaudeRunner,
            crate::work::model::ModelDefaults::none(),
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
            &crate::work::admission::AdmissionDeps::off(),
            &store,
            &tmux,
            &no_lanes(),
            &audit_cfg,
            &home,
            &test_identity(),
            &ClaudeRunner,
            crate::work::model::ModelDefaults::none(),
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
            &crate::work::admission::AdmissionDeps::off(),
            &store,
            &tmux,
            &no_lanes(),
            &audit_cfg,
            &home,
            &test_identity(),
            &ClaudeRunner,
            crate::work::model::ModelDefaults::none(),
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
                    command: "claude --model 'opus' '/ticket-audit PROJ-1'".to_string(),
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
            &crate::work::admission::AdmissionDeps::off(),
            &store,
            &tmux,
            &no_lanes(),
            &audit_cfg,
            &home,
            &test_identity(),
            &ClaudeRunner,
            crate::work::model::ModelDefaults::none(),
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
            &crate::work::admission::AdmissionDeps::off(),
            &store,
            &tmux,
            &no_lanes(),
            &audit_cfg,
            &home,
            &test_identity(),
            &ClaudeRunner,
            crate::work::model::ModelDefaults::none(),
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
            prompt_file: None,
            model: None,
        };

        launch_audit(
            &crate::work::admission::AdmissionDeps::off(),
            &store,
            &tmux,
            &no_lanes(),
            &audit_cfg,
            &home,
            &test_identity(),
            &ClaudeRunner,
            crate::work::model::ModelDefaults::none(),
            "PROJ-9",
        )
        .unwrap();

        assert_eq!(
            launched_command(&tmux),
            Some("claude --model 'opus' '/custom-audit PROJ-9'".to_string())
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
            prompt_file: None,
            model: Some("opus".to_string()),
        };

        launch_audit(
            &crate::work::admission::AdmissionDeps::off(),
            &store,
            &tmux,
            &no_lanes(),
            &audit_cfg,
            &home,
            &test_identity(),
            &ClaudeRunner,
            crate::work::model::ModelDefaults::none(),
            "PROJ-9",
        )
        .unwrap();

        assert_eq!(
            launched_command(&tmux),
            Some("claude --model 'opus' '/ticket-audit PROJ-9'".to_string())
        );
    }

    /// GitHub issue #143: with no `[work.audit] model`, the session falls
    /// back to `[work] default_model` instead of leaving `--model` out; a
    /// set `[work.audit] model` still wins.
    #[test]
    fn launch_audit_falls_back_to_the_work_default_model() {
        let models = crate::config::ModelsConfig::default();
        let defaults = crate::work::model::ModelDefaults {
            work_default: Some("sonnet"),
            models: &models,
        };
        for (audit_model, expected) in [(None, "sonnet"), (Some("fable"), "fable")] {
            let db_dir = tempdir().unwrap();
            let store = open_store(db_dir.path());
            let tmux = FakeTmuxOps::new();
            let audit_cfg = AuditConfig {
                dir: Some("/repo/axiom".to_string()),
                prompt: None,
                prompt_file: None,
                model: audit_model.map(str::to_string),
            };

            launch_audit(
                &crate::work::admission::AdmissionDeps::off(),
                &store,
                &tmux,
                &no_lanes(),
                &audit_cfg,
                Path::new("/Users/jowi"),
                &test_identity(),
                &ClaudeRunner,
                defaults,
                "PROJ-9",
            )
            .unwrap();

            assert_eq!(
                launched_command(&tmux),
                Some(format!(
                    "claude --model '{expected}' '/ticket-audit PROJ-9'"
                ))
            );
        }
    }

    #[test]
    fn launch_audit_uses_prompt_file_contents_with_key_substitution() {
        let db_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());
        let prompt_dir = tempdir().unwrap();
        let prompt_path = prompt_dir.path().join("audit-prompt.md");
        std::fs::write(&prompt_path, "/custom-audit-file {key}").unwrap();
        let tmux = FakeTmuxOps::new();
        let home = PathBuf::from("/Users/jowi");
        let audit_cfg = AuditConfig {
            dir: Some("/repo/axiom".to_string()),
            prompt: Some("/should-not-be-used".to_string()),
            prompt_file: Some(prompt_path.to_string_lossy().into_owned()),
            model: None,
        };

        launch_audit(
            &crate::work::admission::AdmissionDeps::off(),
            &store,
            &tmux,
            &no_lanes(),
            &audit_cfg,
            &home,
            &test_identity(),
            &ClaudeRunner,
            crate::work::model::ModelDefaults::none(),
            "PROJ-9",
        )
        .unwrap();

        assert_eq!(
            launched_command(&tmux),
            Some("claude --model 'opus' '/custom-audit-file PROJ-9'".to_string())
        );
    }

    #[test]
    fn launch_audit_errors_and_creates_no_run_when_prompt_file_is_missing() {
        let db_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());
        let prompt_dir = tempdir().unwrap();
        let prompt_path = prompt_dir.path().join("missing-prompt.md");
        let tmux = FakeTmuxOps::new();
        let home = PathBuf::from("/Users/jowi");
        let audit_cfg = AuditConfig {
            dir: Some("/repo/axiom".to_string()),
            prompt: None,
            prompt_file: Some(prompt_path.to_string_lossy().into_owned()),
            model: None,
        };

        let err = launch_audit(
            &crate::work::admission::AdmissionDeps::off(),
            &store,
            &tmux,
            &no_lanes(),
            &audit_cfg,
            &home,
            &test_identity(),
            &ClaudeRunner,
            crate::work::model::ModelDefaults::none(),
            "PROJ-9",
        )
        .expect_err("missing prompt file should error");

        match &err {
            AuditLaunchError::PromptFile(inner) => {
                let message = inner.to_string();
                assert!(message.contains("[work.audit].prompt_file"));
                assert!(message.contains(&prompt_path.to_string_lossy().to_string()));
            }
            other => panic!("expected PromptFile, got {other:?}"),
        }
        assert!(
            store.list_runs().unwrap().is_empty(),
            "must not pre-register a run when the prompt file is unreadable"
        );
        assert!(
            tmux.calls().iter().all(|call| !matches!(
                call,
                TmuxCall::NewSessionWithCommand { .. } | TmuxCall::NewWindowWithCommand { .. }
            )),
            "no tmux session/window should have been created, got {:?}",
            tmux.calls()
        );
    }
}
