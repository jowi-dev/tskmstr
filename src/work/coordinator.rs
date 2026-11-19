//! Epic coordinator sessions (GitHub issue #209).
//!
//! A coordinator is one interactive agent session per epic. It supervises
//! the lanes that work the epic's tickets: tm is its hands (it launches,
//! inspects and merges lanes only through `tm` commands, so every gate still
//! applies) and thatch chat is its voice. tm's part is starting and stopping
//! the session; what the session does lives in its prompt. See
//! `docs/plans/gh-209-epic-coordinator.md`.
//!
//! [`launch_coordinator`] follows [`crate::work::walkthrough`]'s launch
//! sequence (refuse a double launch, admit, pre-register a run row, start
//! the window with the run id in [`SESSION_RUN_ID_ENV`]) with two
//! differences. It runs in the invoking repo's checkout, because a
//! coordinator writes no code. And it stamps the row's tmux session and
//! records a [`LaunchRecord`], so chat wake (GitHub issue #224) can type
//! into the coordinator's pane when a lane mails it.
//!
//! Checking that the key is an epic and listing its children need the
//! ticket tracker; [`epic_brief`] does both before the launch.

use std::path::Path;

use thiserror::Error;

use crate::agent::{AgentRunner, InvocationInputs, RunMode, ToolScope};
use crate::config::{BackendIdentity, CoordinatorConfig};
use crate::runs::{FinishRun, RunStatus, RunStore, RunStoreError, StartRun};
use crate::ticketing::TicketingError;
use crate::ticketing::provider::TicketProvider;
use crate::work::audit::{LaunchOutcome, SESSION_RUN_ID_ENV, SHELL_WINDOW_NAME};
use crate::work::hibernate::{LaunchRecord, record_launch};
use crate::work::naming::ticket_session_name;
use crate::work::tmux::{TmuxError, TmuxOps, has_live_window, session_window_names};

/// Name of the tmux window a coordinator's agent runs in, in the epic's
/// ticket session.
pub const COORDINATOR_WINDOW_NAME: &str = "coordinator";

/// `kind`/`lane` value of a coordinator's run row.
pub const COORDINATOR_KIND: &str = "coordinator";

/// Errors returned by [`epic_brief`], [`launch_coordinator`] and
/// [`stop_coordinator`].
#[derive(Debug, Error)]
pub enum CoordinatorError {
    /// The key names a ticket that is not an epic.
    #[error("{key} is not an epic")]
    NotAnEpic {
        /// The ticket key.
        key: String,
    },

    /// The epic's coordinator window is already live.
    #[error("a coordinator is already running: {session_name}:{window_name}")]
    AlreadyRunning {
        /// Name of the tmux session holding the live window.
        session_name: String,
        /// Name of the already-live window.
        window_name: String,
    },

    /// [`stop_coordinator`] found neither a running coordinator row nor a
    /// live coordinator window for the epic.
    #[error("no coordinator is running for {epic}")]
    NotRunning {
        /// The epic key.
        epic: String,
    },

    /// Reading the epic or its children from the tracker failed.
    #[error(transparent)]
    Ticketing(#[from] TicketingError),

    /// The run-state database could not be read or written.
    #[error(transparent)]
    Store(#[from] RunStoreError),

    /// Memory or usage admission refused the session (GitHub issue #149).
    #[error(transparent)]
    Admission(#[from] crate::work::admission::AdmissionError),

    /// Shelling out to `tmux` failed.
    #[error(transparent)]
    Tmux(#[from] TmuxError),

    /// `[work.coordinator].prompt_file` was set but could not be read.
    /// Checked before [`RunStore::start_run`], so a bad path leaves no
    /// orphaned run row.
    #[error(transparent)]
    PromptFile(#[from] crate::work::prompt::PromptFileError),
}

/// Checks that `epic` is an epic and renders its children for the
/// coordinator's prompt: one `KEY  STATUS  SUMMARY` line each, the format
/// `tm ticket children` prints, or `(no child tickets yet)`.
///
/// # Errors
///
/// [`CoordinatorError::NotAnEpic`], or [`CoordinatorError::Ticketing`] when
/// the tracker cannot be read (an unknown key included).
pub fn epic_brief(provider: &dyn TicketProvider, epic: &str) -> Result<String, CoordinatorError> {
    let issue = provider.get_issue(epic).map_err(TicketingError::from)?;
    if !provider.is_epic(&issue) {
        return Err(CoordinatorError::NotAnEpic {
            key: epic.to_string(),
        });
    }
    let children = provider.children(epic).map_err(TicketingError::from)?;
    if children.is_empty() {
        return Ok("(no child tickets yet)".to_string());
    }
    Ok(children
        .iter()
        .map(|child| {
            format!(
                "{}  {}  {}",
                child.key, child.fields.status.name, child.fields.summary
            )
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

/// Substitutes `{epic}` and `{children}` in `template`, producing the
/// prompt the coordinator session opens with.
pub fn coordinator_prompt(template: &str, epic: &str, children: &str) -> String {
    template
        .replace("{epic}", epic)
        .replace("{children}", children)
}

/// What [`launch_coordinator`] reads from and writes to.
pub struct CoordinatorLaunchDeps<'a> {
    /// The run-state store the coordinator's row is written to.
    pub store: &'a RunStore,
    /// Tmux operations (real or fake).
    pub tmux: &'a dyn TmuxOps,
    /// The AI coding agent the session launches; supplies the default
    /// prompt template, the rendered shell command and the resume recipe.
    pub runner: &'a dyn AgentRunner,
    /// The end of the session's model chain after `[work.coordinator]
    /// model` (GitHub issue #143, [`crate::work::model`]).
    pub model_defaults: crate::work::model::ModelDefaults<'a>,
    /// Memory and usage admission for the session's agent (GitHub issue
    /// #149).
    pub admission: crate::work::admission::AdmissionDeps<'a>,
}

/// Everything [`launch_coordinator`] needs to know about this launch.
pub struct CoordinatorLaunchRequest<'a> {
    /// Validated `[work.coordinator]` config.
    pub cfg: &'a CoordinatorConfig,
    /// The invoking user's home directory, for `~`-expanding `prompt_file`.
    pub home: &'a Path,
    /// The invoking repo's [`BackendIdentity`]: scopes the run row and
    /// qualifies the epic's session name (GitHub issue #10).
    pub identity: &'a BackendIdentity,
    /// The epic key, e.g. `GH-176`.
    pub epic: &'a str,
    /// The epic's children, as [`epic_brief`] renders them.
    pub children: &'a str,
    /// The checkout the coordinator runs in: the invoking repo's root.
    pub repo_dir: &'a Path,
}

/// Launches (or refuses to launch) a coordinator session for `req.epic`.
///
/// 1. Refuses with [`CoordinatorError::AlreadyRunning`] when a live
///    [`COORDINATOR_WINDOW_NAME`] window exists in the epic's session.
/// 2. Reads `prompt_file` (if set), then admits the launch as a human one,
///    both before any row exists.
/// 3. Pre-registers a `coordinator` run row rooted at `req.repo_dir`, and
///    starts the window there running the runner's interactive command.
///    Creating the epic's session also provisions its shell window and
///    hands focus back to the coordinator window, as
///    [`crate::work::walkthrough::launch_walkthrough`] does.
/// 4. Stamps the row's tmux session and records its [`LaunchRecord`], so
///    liveness, hibernation and chat wake treat it like any interactive
///    run.
///
/// # Errors
///
/// See [`CoordinatorError`]. Every refusal before step 3 leaves no run row
/// and starts no window.
pub fn launch_coordinator(
    deps: &CoordinatorLaunchDeps<'_>,
    req: &CoordinatorLaunchRequest<'_>,
) -> Result<LaunchOutcome, CoordinatorError> {
    let session_name = ticket_session_name(&req.identity.session_slug(), req.epic);
    let windows = deps.tmux.list_windows()?;
    if has_live_window(&windows, &session_name, COORDINATOR_WINDOW_NAME) {
        return Err(CoordinatorError::AlreadyRunning {
            session_name,
            window_name: COORDINATOR_WINDOW_NAME.to_string(),
        });
    }
    let existing_windows = session_window_names(&windows, &session_name);
    // Only one coordinator may be live, and a dead one's window still
    // holds the name: reuse it rather than suffixing, so `stop` always
    // knows which window to kill.
    if existing_windows
        .iter()
        .any(|w| w == COORDINATOR_WINDOW_NAME)
    {
        deps.tmux
            .kill_window(&session_name, COORDINATOR_WINDOW_NAME)?;
    }
    let window_name = COORDINATOR_WINDOW_NAME.to_string();

    let prompt_file_contents = req
        .cfg
        .prompt_file
        .as_deref()
        .map(|raw| {
            crate::work::prompt::read_prompt_file(raw, req.home, "[work.coordinator].prompt_file")
        })
        .transpose()?;

    let repo = crate::work::run::dir_repo_key(req.repo_dir);
    let mut warning = Vec::new();
    let admitted = deps.admission.admit(
        deps.store,
        &crate::work::admission::LaunchRequest {
            kind: COORDINATOR_KIND,
            agent: deps.runner.name(),
            repo: &repo,
            own_run: None,
        },
        crate::work::admission::LaunchOrigin::Human,
        &mut warning,
    )?;

    let dir = req.repo_dir.to_string_lossy().into_owned();
    let run_id = deps.store.start_run(&StartRun {
        ticket: req.epic.to_string(),
        scope: req.identity.scope(),
        lane: COORDINATOR_KIND.to_string(),
        worktree: dir.clone(),
        branch: None,
        pid: None,
        kind: COORDINATOR_KIND.to_string(),
        log_path: None,
    })?;
    deps.store
        .update_agent_repo(run_id, deps.runner.name(), &repo)?;

    let template = prompt_file_contents
        .as_deref()
        .or(req.cfg.prompt.as_deref())
        .unwrap_or_else(|| deps.runner.default_coordinator_prompt_template());
    let prompt = coordinator_prompt(template, req.epic, req.children);
    let model = deps.model_defaults.resolve(
        deps.runner,
        &[crate::work::model::ModelCandidate {
            source: "[work.coordinator] model",
            model: req.cfg.model.as_deref(),
        }],
    );
    let command = deps
        .runner
        .interactive_shell_command(&admitted, &model.model, &prompt);
    let env = [(SESSION_RUN_ID_ENV.to_string(), run_id.to_string())];

    // Killing a dead coordinator window that was the session's last window
    // took the session with it, so only other windows mean it still exists.
    if existing_windows
        .iter()
        .all(|w| w == COORDINATOR_WINDOW_NAME)
    {
        deps.tmux
            .new_session_with_command(&session_name, &dir, &window_name, &env, &command)?;
        deps.tmux
            .new_window(&session_name, SHELL_WINDOW_NAME, &dir)?;
        deps.tmux.select_window(&session_name, &window_name)?;
    } else {
        deps.tmux
            .new_window_with_command(&session_name, &window_name, &dir, &env, &command)?;
    }

    deps.store.update_tmux_session(run_id, &session_name)?;
    // The resume recipe comes from the interactive invocation the runner
    // would build for this prompt and model; the window itself runs
    // `interactive_shell_command`, which carries the same program and model.
    let invocation = deps.runner.build_invocation(
        &admitted,
        InvocationInputs {
            prompt,
            model: model.model.clone(),
            max_turns: None,
            permission_mode: None,
            settings_path: None,
            run_id: None,
            mode: RunMode::Interactive,
            tool_scope: ToolScope::default(),
        },
    );
    record_launch(
        deps.store,
        run_id,
        &LaunchRecord {
            window: window_name.clone(),
            resume: deps.runner.resume_spec(&invocation),
        },
    )?;

    Ok(LaunchOutcome {
        run_id,
        session_name,
        window_name,
        warning: crate::work::admission::warning_text(&warning),
    })
}

/// What [`stop_coordinator`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopOutcome {
    /// The coordinator run row it finished as interrupted, if one was
    /// running.
    pub run_id: Option<i64>,
    /// Whether it killed a live coordinator window.
    pub window_killed: bool,
}

/// Stops `epic`'s coordinator: kills the [`COORDINATOR_WINDOW_NAME`] window
/// in the epic's session and finishes the epic's running `coordinator` row
/// as [`RunStatus::Interrupted`]. Lanes the coordinator launched keep
/// running; they are ordinary lane runs.
///
/// # Errors
///
/// [`CoordinatorError::NotRunning`] when there is neither a running row nor
/// a live window; otherwise a store or tmux failure.
pub fn stop_coordinator(
    store: &RunStore,
    tmux: &dyn TmuxOps,
    identity: &BackendIdentity,
    epic: &str,
) -> Result<StopOutcome, CoordinatorError> {
    let session_name = ticket_session_name(&identity.session_slug(), epic);
    let windows = tmux.list_windows()?;
    let window_live = has_live_window(&windows, &session_name, COORDINATOR_WINDOW_NAME);
    let run = store
        .latest_run_for_ticket_kind(Some(&identity.scope()), epic, Some(COORDINATOR_KIND))?
        .filter(|run| run.status == RunStatus::Running);
    if !window_live && run.is_none() {
        return Err(CoordinatorError::NotRunning {
            epic: epic.to_string(),
        });
    }
    if window_live {
        tmux.kill_window(&session_name, COORDINATOR_WINDOW_NAME)?;
    }
    let run_id = match run {
        Some(run) => {
            store.finish_run(
                run.id,
                &FinishRun {
                    status: RunStatus::Interrupted,
                    ..FinishRun::default()
                },
            )?;
            Some(run.id)
        }
        None => None,
    };
    Ok(StopOutcome {
        run_id,
        window_killed: window_live,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::claude::ClaudeRunner;
    use crate::jira::fake::FakeJiraClient;
    use crate::ticketing::types::{Issue, IssueFields, Status, StatusCategory};
    use crate::work::tmux::{FakeTmuxOps, TmuxCall, TmuxWindow};
    use std::path::PathBuf;
    use tempfile::{TempDir, tempdir};

    const SESSION: &str = "tm-proj-proj-1";

    fn issue(key: &str, summary: &str) -> Issue {
        Issue {
            key: key.to_string(),
            fields: IssueFields {
                summary: summary.to_string(),
                status: Status {
                    name: "To Do".to_string(),
                    status_category: StatusCategory {
                        key: "new".to_string(),
                    },
                },
                description: None,
                assignee: None,
                issue_links: vec![],
                parent: None,
                labels: Vec::new(),
                issue_type: None,
            },
        }
    }

    /// Canonical test identity; ticket sessions are `tm-proj-<key>`.
    fn test_identity() -> &'static BackendIdentity {
        static IDENTITY: std::sync::OnceLock<BackendIdentity> = std::sync::OnceLock::new();
        IDENTITY.get_or_init(|| BackendIdentity::Jira {
            base_url: "https://x.atlassian.net".to_string(),
            project_key: "PROJ".to_string(),
        })
    }

    struct Fixture {
        _db: TempDir,
        repo: TempDir,
        store: RunStore,
        home: PathBuf,
        cfg: CoordinatorConfig,
    }

    fn fixture() -> Fixture {
        let db = tempdir().unwrap();
        let store = RunStore::open(&db.path().join("runs.db")).unwrap();
        Fixture {
            _db: db,
            repo: tempdir().unwrap(),
            store,
            home: PathBuf::from("/Users/jowi"),
            cfg: CoordinatorConfig::default(),
        }
    }

    fn launch(
        fx: &Fixture,
        tmux: &FakeTmuxOps,
        admission: crate::work::admission::AdmissionDeps<'_>,
    ) -> Result<LaunchOutcome, CoordinatorError> {
        let deps = CoordinatorLaunchDeps {
            store: &fx.store,
            tmux,
            runner: &ClaudeRunner,
            model_defaults: crate::work::model::ModelDefaults::none(),
            admission,
        };
        let req = CoordinatorLaunchRequest {
            cfg: &fx.cfg,
            home: &fx.home,
            identity: test_identity(),
            epic: "PROJ-1",
            children: "PROJ-2  To Do  First part",
            repo_dir: fx.repo.path(),
        };
        launch_coordinator(&deps, &req)
    }

    fn window(name: &str, dead: bool) -> TmuxWindow {
        TmuxWindow {
            session: SESSION.to_string(),
            name: name.to_string(),
            dead,
        }
    }

    // --- epic_brief ---

    #[test]
    fn epic_brief_lists_one_line_per_child() {
        let jira = FakeJiraClient::new()
            .with_epic("PROJ-1", issue("PROJ-1", "Epic"))
            .with_issue("PROJ-2", issue("PROJ-2", "First part"))
            .with_issue("PROJ-3", issue("PROJ-3", "Second part"))
            .with_parent("PROJ-2", "PROJ-1")
            .with_parent("PROJ-3", "PROJ-1");

        assert_eq!(
            epic_brief(&jira, "PROJ-1").unwrap(),
            "PROJ-2  To Do  First part\nPROJ-3  To Do  Second part"
        );
    }

    #[test]
    fn epic_brief_of_an_empty_epic_says_so() {
        let jira = FakeJiraClient::new().with_epic("PROJ-1", issue("PROJ-1", "Epic"));
        assert_eq!(
            epic_brief(&jira, "PROJ-1").unwrap(),
            "(no child tickets yet)"
        );
    }

    #[test]
    fn epic_brief_refuses_a_ticket_that_is_not_an_epic() {
        let jira = FakeJiraClient::new().with_issue("PROJ-2", issue("PROJ-2", "Task"));
        let err = epic_brief(&jira, "PROJ-2").unwrap_err();
        assert_eq!(err.to_string(), "PROJ-2 is not an epic");
    }

    // --- prompt ---

    #[test]
    fn coordinator_rows_count_against_memory_admission() {
        assert!(crate::runs::AGENT_KINDS.contains(&COORDINATOR_KIND));
    }

    #[test]
    fn coordinator_prompt_substitutes_epic_and_children() {
        assert_eq!(
            coordinator_prompt("run {epic}:\n{children}", "PROJ-1", "PROJ-2  To Do  x"),
            "run PROJ-1:\nPROJ-2  To Do  x"
        );
    }

    #[test]
    fn default_prompt_dispatches_through_tm_and_supervises_over_chat() {
        let prompt = coordinator_prompt(
            ClaudeRunner.default_coordinator_prompt_template(),
            "PROJ-1",
            "PROJ-2  To Do  First part",
        );
        for needle in [
            "PROJ-1",
            "PROJ-2  To Do  First part",
            "tm ready",
            "tm work run --unattended",
            "tm merge",
            "tm runs",
            "chat",
        ] {
            assert!(prompt.contains(needle), "missing {needle:?} in:\n{prompt}");
        }
        assert!(
            !prompt.contains('{'),
            "unsubstituted placeholder:\n{prompt}"
        );
    }

    // --- launch ---

    #[test]
    fn launches_a_new_session_in_the_repo_checkout() {
        let fx = fixture();
        let tmux = FakeTmuxOps::new();

        let outcome = launch(&fx, &tmux, crate::work::admission::AdmissionDeps::off()).unwrap();

        let dir = fx.repo.path().to_string_lossy().into_owned();
        assert_eq!(outcome.session_name, SESSION);
        assert_eq!(outcome.window_name, COORDINATOR_WINDOW_NAME);
        let run = fx.store.run_by_id(outcome.run_id).unwrap().unwrap();
        assert_eq!(run.ticket, "PROJ-1");
        assert_eq!(run.kind, COORDINATOR_KIND);
        assert_eq!(run.lane, COORDINATOR_KIND);
        assert_eq!(run.status, RunStatus::Running);
        assert_eq!(run.worktree, dir);
        assert_eq!(run.branch, None);
        assert_eq!(run.agent.as_deref(), Some("claude"));
        assert_eq!(run.tmux_session.as_deref(), Some(SESSION));

        let prompt = coordinator_prompt(
            ClaudeRunner.default_coordinator_prompt_template(),
            "PROJ-1",
            "PROJ-2  To Do  First part",
        );
        let admitted = crate::work::admission::Admitted::for_test();
        assert_eq!(
            tmux.calls(),
            vec![
                TmuxCall::ListWindows,
                TmuxCall::NewSessionWithCommand {
                    name: SESSION.to_string(),
                    dir: dir.clone(),
                    window_name: COORDINATOR_WINDOW_NAME.to_string(),
                    env: vec![(SESSION_RUN_ID_ENV.to_string(), outcome.run_id.to_string())],
                    command: ClaudeRunner.interactive_shell_command(&admitted, "opus", &prompt),
                },
                TmuxCall::NewWindow {
                    name: SESSION.to_string(),
                    window_name: SHELL_WINDOW_NAME.to_string(),
                    dir,
                },
                TmuxCall::SelectWindow {
                    name: SESSION.to_string(),
                    window: COORDINATOR_WINDOW_NAME.to_string(),
                },
            ]
        );
    }

    #[test]
    fn records_a_launch_so_chat_wake_can_reach_the_pane() {
        let fx = fixture();
        let tmux = FakeTmuxOps::new();

        let outcome = launch(&fx, &tmux, crate::work::admission::AdmissionDeps::off()).unwrap();

        let record = crate::work::hibernate::launch_record(&fx.store, outcome.run_id)
            .unwrap()
            .expect("a launch record");
        assert_eq!(record.window, COORDINATOR_WINDOW_NAME);
        assert_eq!(record.resume.program, "claude");
    }

    #[test]
    fn a_custom_prompt_replaces_the_default() {
        let mut fx = fixture();
        fx.cfg.prompt = Some("/coordinate {epic}".to_string());
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
        assert!(command.contains("/coordinate PROJ-1"), "{command}");
    }

    #[test]
    fn appends_a_window_when_the_epic_session_exists() {
        let fx = fixture();
        let tmux = FakeTmuxOps::new().with_list_windows(Ok(vec![window("shell", false)]));

        launch(&fx, &tmux, crate::work::admission::AdmissionDeps::off()).unwrap();

        assert!(tmux.calls().iter().any(|call| matches!(
            call,
            TmuxCall::NewWindowWithCommand { window_name, .. } if window_name == COORDINATOR_WINDOW_NAME
        )));
    }

    #[test]
    fn a_dead_coordinator_window_is_replaced_under_the_same_name() {
        let fx = fixture();
        let tmux = FakeTmuxOps::new().with_list_windows(Ok(vec![
            window("shell", false),
            window(COORDINATOR_WINDOW_NAME, true),
        ]));

        let outcome = launch(&fx, &tmux, crate::work::admission::AdmissionDeps::off()).unwrap();

        assert_eq!(outcome.window_name, COORDINATOR_WINDOW_NAME);
        let calls = tmux.calls();
        assert!(calls.contains(&TmuxCall::KillWindow {
            name: SESSION.to_string(),
            window: COORDINATOR_WINDOW_NAME.to_string(),
        }));
        assert!(calls.iter().any(|call| matches!(
            call,
            TmuxCall::NewWindowWithCommand { window_name, .. } if window_name == COORDINATOR_WINDOW_NAME
        )));
    }

    #[test]
    fn refuses_a_double_launch_without_a_row() {
        let fx = fixture();
        let tmux =
            FakeTmuxOps::new().with_list_windows(Ok(vec![window(COORDINATOR_WINDOW_NAME, false)]));

        let err = launch(&fx, &tmux, crate::work::admission::AdmissionDeps::off()).unwrap_err();

        assert_eq!(
            err.to_string(),
            format!("a coordinator is already running: {SESSION}:{COORDINATOR_WINDOW_NAME}")
        );
        assert!(fx.store.list_runs().unwrap().is_empty());
    }

    #[test]
    fn an_over_budget_launch_is_refused_before_its_row() {
        let fx = fixture();
        let tmux = FakeTmuxOps::new();

        let err = launch(
            &fx,
            &tmux,
            crate::work::admission::AdmissionDeps::with_budget(1),
        )
        .unwrap_err();

        assert!(err.to_string().contains("memory budget exceeded"), "{err}");
        assert!(fx.store.list_runs().unwrap().is_empty());
        assert!(
            !tmux
                .calls()
                .iter()
                .any(|c| matches!(c, TmuxCall::NewSessionWithCommand { .. }))
        );
    }

    // --- stop ---

    #[test]
    fn stop_kills_the_window_and_interrupts_the_row() {
        let fx = fixture();
        let outcome = launch(
            &fx,
            &FakeTmuxOps::new(),
            crate::work::admission::AdmissionDeps::off(),
        )
        .unwrap();
        let tmux =
            FakeTmuxOps::new().with_list_windows(Ok(vec![window(COORDINATOR_WINDOW_NAME, false)]));

        let stopped = stop_coordinator(&fx.store, &tmux, test_identity(), "PROJ-1").unwrap();

        assert_eq!(
            stopped,
            StopOutcome {
                run_id: Some(outcome.run_id),
                window_killed: true,
            }
        );
        assert!(tmux.calls().contains(&TmuxCall::KillWindow {
            name: SESSION.to_string(),
            window: COORDINATOR_WINDOW_NAME.to_string(),
        }));
        let run = fx.store.run_by_id(outcome.run_id).unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Interrupted);
    }

    #[test]
    fn stop_finishes_a_running_row_whose_window_is_gone() {
        let fx = fixture();
        let outcome = launch(
            &fx,
            &FakeTmuxOps::new(),
            crate::work::admission::AdmissionDeps::off(),
        )
        .unwrap();
        let tmux = FakeTmuxOps::new();

        let stopped = stop_coordinator(&fx.store, &tmux, test_identity(), "PROJ-1").unwrap();

        assert_eq!(stopped.run_id, Some(outcome.run_id));
        assert!(!stopped.window_killed);
        assert!(
            !tmux
                .calls()
                .iter()
                .any(|c| matches!(c, TmuxCall::KillWindow { .. }))
        );
    }

    #[test]
    fn stop_with_nothing_running_says_so() {
        let fx = fixture();
        let err = stop_coordinator(&fx.store, &FakeTmuxOps::new(), test_identity(), "PROJ-1")
            .unwrap_err();
        assert_eq!(err.to_string(), "no coordinator is running for PROJ-1");
    }
}
