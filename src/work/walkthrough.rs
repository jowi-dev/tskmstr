//! Board-launched change walkthroughs (GitHub issue #210).
//!
//! A walkthrough is an interactive agent session opened in a lane's existing
//! worktree, prompted to run the `thatch-change-walkthrough` skill over the
//! lane's pull request, so the operator reviews a lane's output starting
//! from an explanation instead of a raw diff.
//!
//! The launch sequence is [`crate::work::bugbot::launch_cleanup`]'s: refuse
//! a double launch from one `list_windows` snapshot, admit, pre-register a
//! run row, then start the window with the run id in
//! [`SESSION_RUN_ID_ENV`] so the session adopts the row. The difference is
//! where the session runs and what it needs first. A cleanup session runs in
//! a configured directory; a walkthrough runs in the ticket's lane worktree
//! and needs the lane's PR, both read from the ticket's latest `lane` run
//! row. No network call is made: the PR URL is the one the lane recorded
//! when it finished (see [`crate::work::lane_await`]), so a lane that has
//! not opened its PR yet is refused with [`WalkthroughLaunchError::NoPr`].

use std::path::Path;

use thiserror::Error;

use crate::agent::AgentRunner;
use crate::config::{BackendIdentity, WalkthroughConfig};
use crate::runs::{RunStore, RunStoreError, StartRun};
use crate::work::audit::{LaunchOutcome, SESSION_RUN_ID_ENV, SHELL_WINDOW_NAME};
use crate::work::naming::ticket_session_name;
use crate::work::tmux::{
    TmuxError, TmuxOps, has_live_window, session_window_names, unique_window_name,
};

/// Name of the tmux window a walkthrough session's agent runs in, and the
/// `kind`/`lane` value of its pre-registered run row. One name serves both
/// because [`crate::work::session::action_window_for_kind`] falls back to
/// the kind itself.
pub const WALKTHROUGH_WINDOW_NAME: &str = "walkthrough";

/// `kind`/`lane` value [`launch_walkthrough`] stores for the pre-registered
/// run row.
pub const WALKTHROUGH_KIND: &str = "walkthrough";

/// Errors returned by [`launch_walkthrough`]. The first three are the
/// "nothing to walk through yet" reasons the board shows on its status line.
#[derive(Debug, Error)]
pub enum WalkthroughLaunchError {
    /// The ticket has no lane run, so there is no worktree or PR.
    #[error("no lane run for {key} -- press w first")]
    NoLaneRun {
        /// The ticket key.
        key: String,
    },

    /// The ticket's latest lane run has no recorded PR URL: it is still
    /// running, or it ended without opening one.
    #[error("no PR recorded for {key}'s lane run yet")]
    NoPr {
        /// The ticket key.
        key: String,
    },

    /// The lane run's worktree has since been removed (`tm work remove`).
    #[error("worktree {worktree} for {key} no longer exists")]
    WorktreeMissing {
        /// The ticket key.
        key: String,
        /// The worktree path the lane run recorded.
        worktree: String,
    },

    /// This ticket's walkthrough window is already live. Window-scoped for
    /// the same reason [`crate::work::audit::AuditLaunchError::AlreadyRunning`]
    /// is.
    #[error("a walkthrough is already running: {session_name}:{window_name}")]
    AlreadyRunning {
        /// Name of the tmux session holding the live window.
        session_name: String,
        /// Name of the already-live window.
        window_name: String,
    },

    /// The run-state database could not be read or written.
    #[error(transparent)]
    Store(#[from] RunStoreError),

    /// Memory or usage admission refused the session (GitHub issue #149).
    #[error(transparent)]
    Admission(#[from] crate::work::admission::AdmissionError),

    /// Shelling out to `tmux` failed.
    #[error(transparent)]
    Tmux(#[from] TmuxError),

    /// `[work.walkthrough].prompt_file` was set but could not be read.
    /// Checked before [`RunStore::start_run`], so a bad path leaves no
    /// orphaned run row.
    #[error(transparent)]
    PromptFile(#[from] crate::work::prompt::PromptFileError),
}

/// Substitutes `{key}` and `{pr}` in `template`, producing the prompt the
/// walkthrough session opens with.
pub fn walkthrough_prompt(template: &str, key: &str, pr_url: &str) -> String {
    template.replace("{key}", key).replace("{pr}", pr_url)
}

/// What [`launch_walkthrough`] reads from and writes to.
pub struct WalkthroughLaunchDeps<'a> {
    /// The run-state store the lane run is read from and the walkthrough's
    /// row is written to.
    pub store: &'a RunStore,
    /// Tmux operations (real or fake).
    pub tmux: &'a dyn TmuxOps,
    /// The AI coding agent the session launches; supplies the default
    /// prompt template and the rendered shell command.
    pub runner: &'a dyn AgentRunner,
    /// The end of the session's model chain after `[work.walkthrough]
    /// model` (GitHub issue #143, [`crate::work::model`]).
    pub model_defaults: crate::work::model::ModelDefaults<'a>,
    /// Memory and usage admission for the session's agent (GitHub issue
    /// #149).
    pub admission: crate::work::admission::AdmissionDeps<'a>,
}

/// Everything [`launch_walkthrough`] needs to know about this launch.
pub struct WalkthroughLaunchRequest<'a> {
    /// Validated `[work.walkthrough]` config.
    pub cfg: &'a WalkthroughConfig,
    /// The invoking user's home directory, for `~`-expanding `prompt_file`.
    pub home: &'a Path,
    /// The invoking repo's [`BackendIdentity`]: scopes the lane-run lookup
    /// and qualifies the ticket's session name (GitHub issue #10).
    pub identity: &'a BackendIdentity,
    /// The ticket key, e.g. `PROJ-372`.
    pub key: &'a str,
}

/// Launches (or refuses to launch) a walkthrough session for `req.key`.
///
/// 1. Reads the ticket's latest `lane` run in `req.identity`'s scope and
///    refuses with [`WalkthroughLaunchError::NoLaneRun`],
///    [`WalkthroughLaunchError::NoPr`], or
///    [`WalkthroughLaunchError::WorktreeMissing`] when there is nothing to
///    walk through.
/// 2. Refuses with [`WalkthroughLaunchError::AlreadyRunning`] when a live
///    [`WALKTHROUGH_WINDOW_NAME`] window exists in the ticket's session.
/// 3. Reads `prompt_file` (if set), then admits the launch as a human one,
///    both before any row exists.
/// 4. Pre-registers a `walkthrough` run row rooted at the lane's worktree
///    and branch, and starts the window there running the runner's
///    interactive command. Creating the ticket's session also provisions its
///    shell window and hands focus back to the walkthrough window, as
///    [`crate::work::audit::launch_audit`] does.
///
/// # Errors
///
/// See [`WalkthroughLaunchError`]. Every refusal before step 4 leaves no run
/// row and starts no window.
pub fn launch_walkthrough(
    deps: &WalkthroughLaunchDeps<'_>,
    req: &WalkthroughLaunchRequest<'_>,
) -> Result<LaunchOutcome, WalkthroughLaunchError> {
    let scope = req.identity.scope();
    let lane_run = deps
        .store
        .latest_run_for_ticket_kind(Some(&scope), req.key, Some("lane"))?
        .ok_or_else(|| WalkthroughLaunchError::NoLaneRun {
            key: req.key.to_string(),
        })?;
    let pr_url = lane_run
        .pr_url
        .clone()
        .ok_or_else(|| WalkthroughLaunchError::NoPr {
            key: req.key.to_string(),
        })?;
    let worktree = lane_run.worktree.clone();
    if !Path::new(&worktree).is_dir() {
        return Err(WalkthroughLaunchError::WorktreeMissing {
            key: req.key.to_string(),
            worktree,
        });
    }

    let session_name = ticket_session_name(&req.identity.session_slug(), req.key);
    let windows = deps.tmux.list_windows()?;
    if has_live_window(&windows, &session_name, WALKTHROUGH_WINDOW_NAME) {
        return Err(WalkthroughLaunchError::AlreadyRunning {
            session_name,
            window_name: WALKTHROUGH_WINDOW_NAME.to_string(),
        });
    }
    let existing_windows = session_window_names(&windows, &session_name);
    let window_name = unique_window_name(WALKTHROUGH_WINDOW_NAME, &existing_windows);

    let prompt_file_contents = req
        .cfg
        .prompt_file
        .as_deref()
        .map(|raw| {
            crate::work::prompt::read_prompt_file(raw, req.home, "[work.walkthrough].prompt_file")
        })
        .transpose()?;

    // The lane's own estimate key when it recorded one, so the walkthrough
    // is costed against the repo it reads.
    let repo = lane_run
        .repo
        .clone()
        .unwrap_or_else(|| crate::work::run::dir_repo_key(Path::new(&worktree)));
    let mut warning = Vec::new();
    let admitted = deps.admission.admit(
        deps.store,
        &crate::work::admission::LaunchRequest {
            kind: WALKTHROUGH_KIND,
            agent: deps.runner.name(),
            repo: &repo,
            own_run: None,
        },
        crate::work::admission::LaunchOrigin::Human,
        &mut warning,
    )?;

    let run_id = deps.store.start_run(&StartRun {
        ticket: req.key.to_string(),
        scope,
        lane: WALKTHROUGH_KIND.to_string(),
        worktree: worktree.clone(),
        branch: lane_run.branch.clone(),
        pid: None,
        kind: WALKTHROUGH_KIND.to_string(),
        log_path: None,
    })?;
    deps.store
        .update_agent_repo(run_id, deps.runner.name(), &repo)?;

    let template = prompt_file_contents
        .as_deref()
        .or(req.cfg.prompt.as_deref())
        .unwrap_or_else(|| deps.runner.default_walkthrough_prompt_template());
    let prompt = walkthrough_prompt(template, req.key, &pr_url);
    let model = deps.model_defaults.resolve(
        deps.runner,
        &[crate::work::model::ModelCandidate {
            source: "[work.walkthrough] model",
            model: req.cfg.model.as_deref(),
        }],
    );
    let command = deps
        .runner
        .interactive_shell_command(&admitted, &model.model, &prompt);
    let env = [(SESSION_RUN_ID_ENV.to_string(), run_id.to_string())];

    if existing_windows.is_empty() {
        deps.tmux.new_session_with_command(
            &session_name,
            &worktree,
            &window_name,
            &env,
            &command,
        )?;
        deps.tmux
            .new_window(&session_name, SHELL_WINDOW_NAME, &worktree)?;
        deps.tmux.select_window(&session_name, &window_name)?;
    } else {
        deps.tmux.new_window_with_command(
            &session_name,
            &window_name,
            &worktree,
            &env,
            &command,
        )?;
    }

    Ok(LaunchOutcome {
        run_id,
        session_name,
        window_name,
        warning: crate::work::admission::warning_text(&warning),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::claude::ClaudeRunner;
    use crate::runs::{FinishRun, RunStatus};
    use crate::work::tmux::{FakeTmuxOps, TmuxCall, TmuxWindow};
    use std::path::PathBuf;
    use tempfile::{TempDir, tempdir};

    const PR: &str = "https://github.com/a/b/pull/7";

    fn open_store(dir: &Path) -> RunStore {
        RunStore::open(&dir.join("runs.db")).expect("open should succeed")
    }

    /// Canonical test identity; ticket sessions are `tm-proj-<key>`.
    fn test_identity() -> &'static BackendIdentity {
        static IDENTITY: std::sync::OnceLock<BackendIdentity> = std::sync::OnceLock::new();
        IDENTITY.get_or_init(|| BackendIdentity::Jira {
            base_url: "https://x.atlassian.net".to_string(),
            project_key: "PROJ".to_string(),
        })
    }

    /// Records a finished lane run for `PROJ-1` rooted at `worktree`, with
    /// `pr_url` as its recorded PR.
    fn lane_run(store: &RunStore, worktree: &Path, pr_url: Option<&str>) -> i64 {
        let id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                scope: test_identity().scope(),
                lane: "backend".to_string(),
                worktree: worktree.to_string_lossy().into_owned(),
                branch: Some("proj-1".to_string()),
                pid: None,
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();
        store.update_agent_repo(id, "claude", "tskmstr").unwrap();
        store
            .finish_run(
                id,
                &FinishRun {
                    status: RunStatus::Done,
                    pr_url: pr_url.map(str::to_string),
                    ..FinishRun::default()
                },
            )
            .unwrap();
        id
    }

    struct Fixture {
        _db: TempDir,
        worktree: TempDir,
        store: RunStore,
        home: PathBuf,
        cfg: WalkthroughConfig,
    }

    fn fixture() -> Fixture {
        let db = tempdir().unwrap();
        let store = open_store(db.path());
        Fixture {
            _db: db,
            worktree: tempdir().unwrap(),
            store,
            home: PathBuf::from("/Users/jowi"),
            cfg: WalkthroughConfig::default(),
        }
    }

    fn launch(
        fx: &Fixture,
        tmux: &FakeTmuxOps,
        admission: crate::work::admission::AdmissionDeps<'_>,
    ) -> Result<LaunchOutcome, WalkthroughLaunchError> {
        let deps = WalkthroughLaunchDeps {
            store: &fx.store,
            tmux,
            runner: &ClaudeRunner,
            model_defaults: crate::work::model::ModelDefaults::none(),
            admission,
        };
        let req = WalkthroughLaunchRequest {
            cfg: &fx.cfg,
            home: &fx.home,
            identity: test_identity(),
            key: "PROJ-1",
        };
        launch_walkthrough(&deps, &req)
    }

    #[test]
    fn walkthrough_rows_count_against_memory_admission() {
        assert!(crate::runs::AGENT_KINDS.contains(&WALKTHROUGH_KIND));
    }

    #[test]
    fn walkthrough_prompt_substitutes_key_and_pr() {
        assert_eq!(
            walkthrough_prompt("walk {key} through {pr}", "PROJ-1", PR),
            format!("walk PROJ-1 through {PR}")
        );
    }

    #[test]
    fn default_prompt_names_the_skill_and_the_pr() {
        let prompt = walkthrough_prompt(
            ClaudeRunner.default_walkthrough_prompt_template(),
            "PROJ-1",
            PR,
        );
        assert!(prompt.contains("thatch-change-walkthrough"), "{prompt}");
        assert!(prompt.contains(PR), "{prompt}");
        assert!(prompt.contains("PROJ-1"), "{prompt}");
    }

    #[test]
    fn refuses_a_ticket_with_no_lane_run() {
        let fx = fixture();
        let tmux = FakeTmuxOps::new();
        let err = launch(&fx, &tmux, crate::work::admission::AdmissionDeps::off()).unwrap_err();
        assert_eq!(err.to_string(), "no lane run for PROJ-1 -- press w first");
        assert!(tmux.calls().is_empty());
    }

    #[test]
    fn refuses_a_lane_run_with_no_recorded_pr() {
        let fx = fixture();
        lane_run(&fx.store, fx.worktree.path(), None);
        let tmux = FakeTmuxOps::new();
        let err = launch(&fx, &tmux, crate::work::admission::AdmissionDeps::off()).unwrap_err();
        assert_eq!(err.to_string(), "no PR recorded for PROJ-1's lane run yet");
        assert!(tmux.calls().is_empty());
        assert_eq!(fx.store.list_runs().unwrap().len(), 1);
    }

    #[test]
    fn refuses_a_removed_worktree() {
        let fx = fixture();
        let gone = fx.worktree.path().join("gone");
        lane_run(&fx.store, &gone, Some(PR));
        let tmux = FakeTmuxOps::new();
        let err = launch(&fx, &tmux, crate::work::admission::AdmissionDeps::off()).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("worktree {} for PROJ-1 no longer exists", gone.display())
        );
        assert!(tmux.calls().is_empty());
    }

    #[test]
    fn launches_in_the_lane_worktree_with_the_pr_in_the_prompt() {
        let fx = fixture();
        lane_run(&fx.store, fx.worktree.path(), Some(PR));
        let tmux = FakeTmuxOps::new();

        let outcome = launch(&fx, &tmux, crate::work::admission::AdmissionDeps::off()).unwrap();

        let worktree = fx.worktree.path().to_string_lossy().into_owned();
        assert_eq!(outcome.session_name, "tm-proj-proj-1");
        assert_eq!(outcome.window_name, WALKTHROUGH_WINDOW_NAME);
        let run = fx.store.run_by_id(outcome.run_id).unwrap().unwrap();
        assert_eq!(run.kind, WALKTHROUGH_KIND);
        assert_eq!(run.lane, WALKTHROUGH_KIND);
        assert_eq!(run.status, RunStatus::Running);
        assert_eq!(run.worktree, worktree);
        assert_eq!(run.branch.as_deref(), Some("proj-1"));
        assert_eq!(run.agent.as_deref(), Some("claude"));
        assert_eq!(run.repo.as_deref(), Some("tskmstr"));

        let prompt = walkthrough_prompt(
            ClaudeRunner.default_walkthrough_prompt_template(),
            "PROJ-1",
            PR,
        );
        let admitted = crate::work::admission::Admitted::for_test();
        assert_eq!(
            tmux.calls(),
            vec![
                TmuxCall::ListWindows,
                TmuxCall::NewSessionWithCommand {
                    name: "tm-proj-proj-1".to_string(),
                    dir: worktree.clone(),
                    window_name: WALKTHROUGH_WINDOW_NAME.to_string(),
                    env: vec![(SESSION_RUN_ID_ENV.to_string(), outcome.run_id.to_string())],
                    command: ClaudeRunner.interactive_shell_command(&admitted, "opus", &prompt),
                },
                TmuxCall::NewWindow {
                    name: "tm-proj-proj-1".to_string(),
                    window_name: SHELL_WINDOW_NAME.to_string(),
                    dir: worktree,
                },
                TmuxCall::SelectWindow {
                    name: "tm-proj-proj-1".to_string(),
                    window: WALKTHROUGH_WINDOW_NAME.to_string(),
                },
            ]
        );
    }

    #[test]
    fn a_custom_prompt_replaces_the_default() {
        let mut fx = fixture();
        fx.cfg.prompt = Some("/walk {key} {pr}".to_string());
        lane_run(&fx.store, fx.worktree.path(), Some(PR));
        let tmux = FakeTmuxOps::new();

        launch(&fx, &tmux, crate::work::admission::AdmissionDeps::off()).unwrap();

        let command = tmux
            .calls()
            .into_iter()
            .find_map(|call| match call {
                TmuxCall::NewSessionWithCommand { command, .. } => Some(command),
                _ => None,
            })
            .unwrap();
        assert!(command.contains(&format!("/walk PROJ-1 {PR}")), "{command}");
    }

    #[test]
    fn appends_a_window_when_the_ticket_session_exists() {
        let fx = fixture();
        lane_run(&fx.store, fx.worktree.path(), Some(PR));
        let tmux = FakeTmuxOps::new().with_list_windows(Ok(vec![TmuxWindow {
            session: "tm-proj-proj-1".to_string(),
            name: "work".to_string(),
            dead: false,
        }]));

        let outcome = launch(&fx, &tmux, crate::work::admission::AdmissionDeps::off()).unwrap();

        assert!(tmux.calls().iter().any(|call| matches!(
            call,
            TmuxCall::NewWindowWithCommand { window_name, .. } if window_name == WALKTHROUGH_WINDOW_NAME
        )));
        assert_eq!(outcome.window_name, WALKTHROUGH_WINDOW_NAME);
    }

    #[test]
    fn refuses_a_double_launch_without_a_row() {
        let fx = fixture();
        lane_run(&fx.store, fx.worktree.path(), Some(PR));
        let tmux = FakeTmuxOps::new().with_list_windows(Ok(vec![TmuxWindow {
            session: "tm-proj-proj-1".to_string(),
            name: WALKTHROUGH_WINDOW_NAME.to_string(),
            dead: false,
        }]));

        let err = launch(&fx, &tmux, crate::work::admission::AdmissionDeps::off()).unwrap_err();

        assert!(matches!(
            err,
            WalkthroughLaunchError::AlreadyRunning { ref session_name, .. }
                if session_name == "tm-proj-proj-1"
        ));
        assert_eq!(fx.store.list_runs().unwrap().len(), 1);
    }

    #[test]
    fn an_over_budget_launch_is_refused_before_its_row() {
        let fx = fixture();
        lane_run(&fx.store, fx.worktree.path(), Some(PR));
        let tmux = FakeTmuxOps::new();

        let err = launch(
            &fx,
            &tmux,
            crate::work::admission::AdmissionDeps::with_budget(1),
        )
        .unwrap_err();

        assert!(err.to_string().contains("memory budget exceeded"), "{err}");
        assert_eq!(fx.store.list_runs().unwrap().len(), 1);
        assert!(
            !tmux
                .calls()
                .iter()
                .any(|c| matches!(c, TmuxCall::NewSessionWithCommand { .. }))
        );
    }
}
