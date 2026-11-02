//! Board-launched ticket-*creation* sessions (GitHub issue #15): the `c`
//! key's launch half. Like an audit (see [`crate::work::audit`]), a create
//! session is an interactive agent conversation and so is tmux-hosted — but
//! three things set it apart:
//!
//! - **Keyless.** No ticket exists when the session launches, so per-ticket
//!   session naming cannot apply; the whole scope shares one
//!   [`crate::work::naming::create_session_name`] session, and a second
//!   launch while it is live is an [`CreateLaunchError::AlreadyRunning`] the
//!   board answers by attaching rather than duplicating.
//! - **A placeholder ticket on its run row.** Like an audit, the launch
//!   pre-registers a `kind = "create"` row (GitHub issue #156) so the agent
//!   is visible to runs.db, and the memory budget, from the moment `c` is
//!   pressed. No ticket exists yet, so the row carries
//!   [`crate::runs::session::CREATE_PLACEHOLDER_TICKET`]; the session's
//!   first `tm ticket create` adopts it and re-stamps the ticket to the key
//!   it created, and later creates reuse it (see
//!   [`crate::runs::session::register_session`]). A session that never
//!   creates a ticket has its row finished by the SessionEnd hook's create
//!   fallback instead. Unlike every other interactive launch, the prompt
//!   gets no registration preamble: the default prompt is the
//!   `/ticket-create` slash command, which only works as the whole prompt.
//! - **Attached immediately.** The point of the flow is to start dictating a
//!   ticket right away, so the board attaches the moment the launch returns
//!   (that attach is the board's job, not this module's — same split as
//!   audit's launch/attach).

use std::path::Path;

use thiserror::Error;

use crate::agent::AgentRunner;
use crate::config::{BackendIdentity, CreateConfig};
use crate::runs::session::{CREATE_KIND, CREATE_PLACEHOLDER_TICKET};
use crate::runs::{FinishRun, RunStatus, RunStore, StartRun};
use crate::work::audit::SESSION_RUN_ID_ENV;
use crate::work::naming::{create_session_name, expand_tilde};
use crate::work::tmux::{
    TmuxError, TmuxOps, has_live_window, session_window_names, unique_window_name,
};

/// Name of the tmux window a launched create session's agent process runs
/// in.
pub const CREATE_WINDOW_NAME: &str = "create";

/// Errors returned by [`launch_create`].
#[derive(Debug, Error)]
pub enum CreateLaunchError {
    /// `[work.create].dir` is unset, so there is nowhere to launch the
    /// session. A status-line error for the caller to surface, not a crash —
    /// same posture as [`crate::work::audit::AuditLaunchError::NotConfigured`].
    #[error("ticket creation is not configured; set [work.create].dir")]
    NotConfigured,

    /// The scope's create window is already live. The caller should attach
    /// to `session_name` instead of launching a second draft — issue #15's
    /// "deliberate re-entry" criterion.
    #[error("a create session is already running: {session_name}:{window_name}")]
    AlreadyRunning {
        /// Name of the tmux session holding the live window.
        session_name: String,
        /// Name of the already-live window.
        window_name: String,
    },

    /// Shelling out to `tmux` failed.
    #[error(transparent)]
    Tmux(#[from] TmuxError),

    /// `create_cfg.prompt_file` was set but could not be read.
    #[error(transparent)]
    PromptFile(#[from] crate::work::prompt::PromptFileError),

    /// Memory admission refused the session (GitHub issue #149).
    #[error(transparent)]
    Admission(#[from] crate::work::admission::AdmissionError),
}

/// Successful outcome of [`launch_create`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateLaunchOutcome {
    /// Name of the tmux session the caller should attach to.
    pub session_name: String,
    /// Name of the window the agent runs in: [`CREATE_WINDOW_NAME`], or a
    /// [`unique_window_name`] suffixed variant when a previous create
    /// session's dead window still holds that name.
    pub window_name: String,
    /// The pre-registered `create` row, or `None` when no store was given
    /// or registering failed (telemetry never blocks the launch).
    pub run_id: Option<i64>,
    /// Admission's warning for a launch that went ahead anyway (over a
    /// usage ceiling, usage unknown, or no store), for the board's status
    /// line.
    pub warning: Option<String>,
}

/// Launches (or refuses to double-launch) the scope's ticket-creation
/// session.
///
/// 1. Errors with [`CreateLaunchError::NotConfigured`] if `create_cfg.dir`
///    is unset.
/// 2. Errors with [`CreateLaunchError::AlreadyRunning`] if a live window
///    named [`CREATE_WINDOW_NAME`] already exists in the scope's
///    [`create_session_name`] session — the caller attaches to it instead.
/// 3. Otherwise starts the runner's interactive CLI (with the model flag
///    when `create_cfg.model` is set — see
///    [`AgentRunner::interactive_shell_command`]) in that window, prompted
///    with `create_cfg.prompt_file`'s contents (read via
///    [`crate::work::prompt::read_prompt_file`]), else `create_cfg.prompt`,
///    else the runner's
///    [`default_create_prompt`](AgentRunner::default_create_prompt) —
///    unlike [`crate::work::audit`]'s template, no `{key}` substitution
///    applies, as no ticket key exists yet. The prompt file is read after
///    the checks above but before any tmux mutation, so a bad path errors
///    out with no session or window created. The session is created if this
///    is the scope's first create launch, and the window appended to it
///    otherwise, taking a [`unique_window_name`] suffix if a dead
///    predecessor still holds the plain name. An appended window is also
///    selected, so the immediate attach lands on the fresh agent rather
///    than the dead aftermath.
///
/// With a `store`, a `create` row is pre-registered once the checks and
/// the prompt-file read have passed, so a refused launch or a bad path
/// leaves no row: `lane = kind = "create"`, `ticket =`
/// [`CREATE_PLACEHOLDER_TICKET`], `worktree` the create directory, `pid =
/// None`, and `agent`/`repo` the runner's name and the directory's name
/// ([`crate::work::run::dir_repo_key`]). Its id rides into the window as
/// [`SESSION_RUN_ID_ENV`] for the session to adopt. Once the window exists
/// the row is stamped with the tmux session (so killing it reaps the row
/// `interrupted`) and, best-effort, the pane's pid, so a draft left idle
/// before its first `tm ticket create` is not reaped stale. A failed tmux
/// spawn finishes the row `failed`. Store errors are swallowed: a broken
/// runs DB never blocks creating tickets, and the launch then injects no
/// environment, as it does without a store.
///
/// `home` resolves a leading `~` in `create_cfg.dir` via [`expand_tilde`],
/// and `identity`'s [`session_slug`](BackendIdentity::session_slug)
/// qualifies the session name, both exactly as
/// [`crate::work::audit::launch_audit`] does.
///
/// The session's model is `create_cfg.model`, else the rest of the chain
/// `model_defaults` ends (GitHub issue #143, [`crate::work::model`]).
#[allow(clippy::too_many_arguments)]
pub fn launch_create(
    admission: &crate::work::admission::AdmissionDeps<'_>,
    store: Option<&RunStore>,
    tmux: &dyn TmuxOps,
    create_cfg: &CreateConfig,
    home: &Path,
    identity: &BackendIdentity,
    runner: &dyn AgentRunner,
    model_defaults: crate::work::model::ModelDefaults<'_>,
) -> Result<CreateLaunchOutcome, CreateLaunchError> {
    let raw_dir = create_cfg
        .dir
        .as_deref()
        .ok_or(CreateLaunchError::NotConfigured)?;
    let dir = expand_tilde(raw_dir, home);
    let dir_str = dir.to_string_lossy().into_owned();

    let session_name = create_session_name(&identity.session_slug());
    // One `list_windows` snapshot answers both "is a create session already
    // running?" and "does the session exist yet?", mirroring `launch_audit`.
    let windows = tmux.list_windows()?;
    if has_live_window(&windows, &session_name, CREATE_WINDOW_NAME) {
        return Err(CreateLaunchError::AlreadyRunning {
            session_name,
            window_name: CREATE_WINDOW_NAME.to_string(),
        });
    }
    let existing_windows = session_window_names(&windows, &session_name);
    let window_name = unique_window_name(CREATE_WINDOW_NAME, &existing_windows);

    let prompt_file_contents = create_cfg
        .prompt_file
        .as_deref()
        .map(|raw| crate::work::prompt::read_prompt_file(raw, home, "[work.create].prompt_file"))
        .transpose()?;
    let prompt = prompt_file_contents
        .as_deref()
        .or(create_cfg.prompt.as_deref())
        .unwrap_or_else(|| runner.default_create_prompt());
    let model = model_defaults.resolve(
        runner,
        &[crate::work::model::ModelCandidate {
            source: "[work.create] model",
            model: create_cfg.model.as_deref(),
        }],
    );
    // Admission (GitHub issue #149) before the row, so a refusal leaves
    // none. A create session is human-started: over the memory budget it
    // refuses, over a usage ceiling it warns and launches.
    let mut warning = Vec::new();
    let admitted = admission.admit_tracked(
        store,
        &crate::work::admission::LaunchRequest {
            kind: CREATE_KIND,
            agent: runner.name(),
            repo: &crate::work::run::dir_repo_key(&dir),
            own_run: None,
        },
        crate::work::admission::LaunchOrigin::Human,
        &mut warning,
    )?;
    let command = runner.interactive_shell_command(&admitted, &model.model, prompt);

    let run_id = store.and_then(|store| preregister(store, identity, &dir, runner));
    let env: Vec<(String, String)> = run_id
        .map(|id| (SESSION_RUN_ID_ENV.to_string(), id.to_string()))
        .into_iter()
        .collect();

    let spawned = if existing_windows.is_empty() {
        tmux.new_session_with_command(&session_name, &dir_str, &window_name, &env, &command)
    } else {
        tmux.new_window_with_command(&session_name, &window_name, &dir_str, &env, &command)
            .and_then(|()| tmux.select_window(&session_name, &window_name))
    };

    if let (Some(store), Some(id)) = (store, run_id) {
        if let Err(err) = &spawned {
            let _ = store.finish_run(
                id,
                &FinishRun {
                    status: RunStatus::Failed,
                    blocker: Some(format!("create session failed to launch: {err}")),
                    ..FinishRun::default()
                },
            );
        } else {
            let _ = store.update_tmux_session(id, &session_name);
            if let Ok(Some(pid)) = tmux.pane_pid(&session_name, &window_name) {
                let _ = store.update_pid(id, pid);
            }
        }
    }
    spawned?;

    Ok(CreateLaunchOutcome {
        session_name,
        window_name,
        run_id,
        warning: crate::work::admission::warning_text(&warning),
    })
}

/// Starts the create session's placeholder row and stamps its agent and
/// repo, returning its id, or `None` if the store refused (see
/// [`launch_create`] for why that is swallowed).
fn preregister(
    store: &RunStore,
    identity: &BackendIdentity,
    dir: &Path,
    runner: &dyn AgentRunner,
) -> Option<i64> {
    let id = store
        .start_run(&StartRun {
            ticket: CREATE_PLACEHOLDER_TICKET.to_string(),
            scope: identity.scope(),
            lane: CREATE_KIND.to_string(),
            worktree: dir.to_string_lossy().into_owned(),
            branch: None,
            pid: None,
            kind: CREATE_KIND.to_string(),
            log_path: None,
        })
        .ok()?;
    let _ = store.update_agent_repo(id, runner.name(), &crate::work::run::dir_repo_key(dir));
    Some(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runs::RunStatus;
    use crate::runs::session::CREATE_PLACEHOLDER_TICKET;
    use crate::work::tmux::{FakeTmuxOps, TmuxCall, TmuxWindow};
    use std::path::PathBuf;

    fn configured(dir: &str) -> CreateConfig {
        CreateConfig {
            dir: Some(dir.to_string()),
            prompt: None,
            prompt_file: None,
            model: None,
        }
    }

    /// Canonical test identity; its `session_slug()` is `proj`, so the
    /// create session in these tests is named `tm-proj-create`.
    fn test_identity() -> BackendIdentity {
        BackendIdentity::Jira {
            base_url: "https://x.atlassian.net".to_string(),
            project_key: "PROJ".to_string(),
        }
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

    /// The runner every test launches with; command-string assertions below
    /// are claude-shaped because of it.
    const RUNNER: &crate::agent::claude::ClaudeRunner = &crate::agent::claude::ClaudeRunner;

    #[test]
    fn launch_create_starts_a_keyless_session_with_no_env() {
        let tmux = FakeTmuxOps::new();
        let home = PathBuf::from("/Users/jowi");
        let cfg = configured("~/Projects/axiom");

        let outcome = launch_create(
            &crate::work::admission::AdmissionDeps::off(),
            None,
            &tmux,
            &cfg,
            &home,
            &test_identity(),
            RUNNER,
            crate::work::model::ModelDefaults::none(),
        )
        .expect("launch should succeed");

        assert_eq!(outcome.session_name, "tm-proj-create");
        assert_eq!(outcome.window_name, "create");
        assert_eq!(
            tmux.calls(),
            vec![
                TmuxCall::ListWindows,
                TmuxCall::NewSessionWithCommand {
                    name: "tm-proj-create".to_string(),
                    dir: "/Users/jowi/Projects/axiom".to_string(),
                    window_name: "create".to_string(),
                    env: vec![],
                    command: "claude --model 'opus' '/ticket-create'".to_string(),
                },
            ]
        );
    }

    fn open_store(dir: &Path) -> RunStore {
        RunStore::open(&dir.join("runs.db")).expect("open should succeed")
    }

    #[test]
    fn launch_create_over_the_memory_budget_is_refused_before_its_row() {
        let db = tempfile::tempdir().unwrap();
        let store = open_store(db.path());
        let tmux = FakeTmuxOps::new();
        let cfg = configured("/repo/axiom");

        let err = launch_create(
            &crate::work::admission::AdmissionDeps::with_budget(1),
            Some(&store),
            &tmux,
            &cfg,
            Path::new("/Users/jowi"),
            &test_identity(),
            RUNNER,
            crate::work::model::ModelDefaults::none(),
        )
        .unwrap_err();

        assert!(
            err.to_string()
                .starts_with("not launching: memory budget exceeded: axiom/claude create runs"),
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

    /// GitHub issue #156: pressing `c` leaves a running `create` row at
    /// once, before any ticket exists, for the session's first `tm ticket
    /// create` to adopt.
    #[test]
    fn launch_create_preregisters_a_running_create_row() {
        let db = tempfile::tempdir().unwrap();
        let store = open_store(db.path());
        let tmux = FakeTmuxOps::new().with_pane_pid(Ok(Some(4321)));
        let cfg = configured("/repo/axiom");

        let outcome = launch_create(
            &crate::work::admission::AdmissionDeps::off(),
            Some(&store),
            &tmux,
            &cfg,
            Path::new("/Users/jowi"),
            &test_identity(),
            RUNNER,
            crate::work::model::ModelDefaults::none(),
        )
        .unwrap();

        let run_id = outcome.run_id.expect("a row was pre-registered");
        assert_eq!(store.list_runs().unwrap().len(), 1);
        let run = store.run_by_id(run_id).unwrap().unwrap();
        assert_eq!(run.kind, "create");
        assert_eq!(run.lane, "create");
        assert_eq!(run.ticket, CREATE_PLACEHOLDER_TICKET);
        assert_eq!(run.scope, test_identity().scope());
        assert_eq!(run.worktree, "/repo/axiom");
        assert_eq!(run.status, RunStatus::Running);
        assert_eq!(run.agent.as_deref(), Some("claude"));
        assert_eq!(run.repo.as_deref(), Some("axiom"));
        assert_eq!(run.tmux_session.as_deref(), Some("tm-proj-create"));
        assert_eq!(
            run.pid,
            Some(4321),
            "the pane's pid keeps an idle draft from being reaped stale"
        );
        assert_eq!(
            tmux.calls(),
            vec![
                TmuxCall::ListWindows,
                TmuxCall::NewSessionWithCommand {
                    name: "tm-proj-create".to_string(),
                    dir: "/repo/axiom".to_string(),
                    window_name: "create".to_string(),
                    env: vec![("TSKMSTR_SESSION_RUN_ID".to_string(), run_id.to_string())],
                    // No registration preamble: it would break the slash
                    // command the prompt is.
                    command: "claude --model 'opus' '/ticket-create'".to_string(),
                },
                TmuxCall::PanePid {
                    name: "tm-proj-create".to_string(),
                    window: "create".to_string(),
                },
            ]
        );
    }

    #[test]
    fn launch_create_passes_the_session_run_id_to_an_appended_window() {
        let db = tempfile::tempdir().unwrap();
        let store = open_store(db.path());
        let tmux = tmux_with_windows(&[("tm-proj-create", "create", true)]);

        let outcome = launch_create(
            &crate::work::admission::AdmissionDeps::off(),
            Some(&store),
            &tmux,
            &configured("/repo/axiom"),
            Path::new("/Users/jowi"),
            &test_identity(),
            RUNNER,
            crate::work::model::ModelDefaults::none(),
        )
        .unwrap();

        let run_id = outcome.run_id.unwrap();
        let env = tmux.calls().into_iter().find_map(|call| match call {
            TmuxCall::NewWindowWithCommand { env, .. } => Some(env),
            _ => None,
        });
        assert_eq!(
            env,
            Some(vec![(
                "TSKMSTR_SESSION_RUN_ID".to_string(),
                run_id.to_string()
            )])
        );
    }

    #[test]
    fn launch_create_without_a_store_registers_nothing_and_injects_no_env() {
        let tmux = FakeTmuxOps::new();

        let outcome = launch_create(
            &crate::work::admission::AdmissionDeps::off(),
            None,
            &tmux,
            &configured("/repo/axiom"),
            Path::new("/Users/jowi"),
            &test_identity(),
            RUNNER,
            crate::work::model::ModelDefaults::none(),
        )
        .unwrap();

        assert_eq!(outcome.run_id, None);
        assert!(
            !tmux
                .calls()
                .iter()
                .any(|call| matches!(call, TmuxCall::PanePid { .. }))
        );
    }

    #[test]
    fn launch_create_refusing_a_double_launch_registers_no_row() {
        let db = tempfile::tempdir().unwrap();
        let store = open_store(db.path());
        let tmux = tmux_with_windows(&[("tm-proj-create", "create", false)]);

        let result = launch_create(
            &crate::work::admission::AdmissionDeps::off(),
            Some(&store),
            &tmux,
            &configured("/repo/axiom"),
            Path::new("/Users/jowi"),
            &test_identity(),
            RUNNER,
            crate::work::model::ModelDefaults::none(),
        );

        assert!(matches!(
            result,
            Err(CreateLaunchError::AlreadyRunning { .. })
        ));
        assert!(store.list_runs().unwrap().is_empty());
    }

    #[test]
    fn launch_create_with_a_missing_prompt_file_registers_no_row() {
        let db = tempfile::tempdir().unwrap();
        let store = open_store(db.path());
        let tmux = FakeTmuxOps::new();
        let cfg = CreateConfig {
            prompt_file: Some(db.path().join("missing.md").to_string_lossy().into_owned()),
            ..configured("/repo/axiom")
        };

        let result = launch_create(
            &crate::work::admission::AdmissionDeps::off(),
            Some(&store),
            &tmux,
            &cfg,
            Path::new("/Users/jowi"),
            &test_identity(),
            RUNNER,
            crate::work::model::ModelDefaults::none(),
        );

        assert!(matches!(result, Err(CreateLaunchError::PromptFile(_))));
        assert!(store.list_runs().unwrap().is_empty());
    }

    #[test]
    fn launch_create_errors_when_dir_is_unset() {
        let tmux = FakeTmuxOps::new();
        let home = PathBuf::from("/Users/jowi");
        let cfg = CreateConfig::default();

        let err = launch_create(
            &crate::work::admission::AdmissionDeps::off(),
            None,
            &tmux,
            &cfg,
            &home,
            &test_identity(),
            RUNNER,
            crate::work::model::ModelDefaults::none(),
        )
        .expect_err("should refuse to launch");

        assert!(matches!(err, CreateLaunchError::NotConfigured));
        assert!(tmux.calls().is_empty());
    }

    #[test]
    fn launch_create_errors_when_the_create_window_is_live() {
        let tmux = tmux_with_windows(&[("tm-proj-create", "create", false)]);
        let home = PathBuf::from("/Users/jowi");
        let cfg = configured("/repo/axiom");

        let err = launch_create(
            &crate::work::admission::AdmissionDeps::off(),
            None,
            &tmux,
            &cfg,
            &home,
            &test_identity(),
            RUNNER,
            crate::work::model::ModelDefaults::none(),
        )
        .expect_err("should refuse to double-launch");

        match err {
            CreateLaunchError::AlreadyRunning {
                session_name,
                window_name,
            } => {
                assert_eq!(session_name, "tm-proj-create");
                assert_eq!(window_name, "create");
            }
            other => panic!("expected AlreadyRunning, got {other:?}"),
        }
    }

    #[test]
    fn launch_create_relaunches_over_a_dead_window_with_a_suffix_and_selects_it() {
        // A window whose pane exited is aftermath, not a running draft: the
        // relaunch appends a suffixed window and selects it so the immediate
        // attach lands on the fresh claude.
        let tmux = tmux_with_windows(&[("tm-proj-create", "create", true)]);
        let home = PathBuf::from("/Users/jowi");
        let cfg = configured("/repo/axiom");

        let outcome = launch_create(
            &crate::work::admission::AdmissionDeps::off(),
            None,
            &tmux,
            &cfg,
            &home,
            &test_identity(),
            RUNNER,
            crate::work::model::ModelDefaults::none(),
        )
        .expect("launch should succeed");

        assert_eq!(outcome.window_name, "create-2");
        assert_eq!(
            tmux.calls(),
            vec![
                TmuxCall::ListWindows,
                TmuxCall::NewWindowWithCommand {
                    name: "tm-proj-create".to_string(),
                    window_name: "create-2".to_string(),
                    dir: "/repo/axiom".to_string(),
                    env: vec![],
                    command: "claude --model 'opus' '/ticket-create'".to_string(),
                },
                TmuxCall::SelectWindow {
                    name: "tm-proj-create".to_string(),
                    window: "create-2".to_string(),
                },
            ]
        );
    }

    #[test]
    fn launch_create_ignores_other_scopes_create_sessions() {
        // Another repo's create session must not read as "already running"
        // here — the scope slug is what keeps them apart (issue #10's rule).
        let tmux = tmux_with_windows(&[("tm-other-create", "create", false)]);
        let home = PathBuf::from("/Users/jowi");
        let cfg = configured("/repo/axiom");

        let outcome = launch_create(
            &crate::work::admission::AdmissionDeps::off(),
            None,
            &tmux,
            &cfg,
            &home,
            &test_identity(),
            RUNNER,
            crate::work::model::ModelDefaults::none(),
        )
        .expect("launch should succeed");

        assert_eq!(outcome.session_name, "tm-proj-create");
    }

    /// GitHub issue #143: with no `[work.create] model`, the session uses
    /// `[work] default_model`.
    #[test]
    fn launch_create_falls_back_to_the_work_default_model() {
        let tmux = FakeTmuxOps::new();
        let cfg = CreateConfig {
            dir: Some("/repo/axiom".to_string()),
            prompt: None,
            prompt_file: None,
            model: None,
        };
        let models = crate::config::ModelsConfig::default();
        let defaults = crate::work::model::ModelDefaults {
            work_default: Some("sonnet"),
            models: &models,
        };

        launch_create(
            &crate::work::admission::AdmissionDeps::off(),
            None,
            &tmux,
            &cfg,
            Path::new("/Users/jowi"),
            &test_identity(),
            RUNNER,
            defaults,
        )
        .unwrap();

        let command = tmux.calls().iter().find_map(|call| match call {
            TmuxCall::NewSessionWithCommand { command, .. } => Some(command.clone()),
            _ => None,
        });
        assert_eq!(
            command,
            Some("claude --model 'sonnet' '/ticket-create'".to_string())
        );
    }

    #[test]
    fn launch_create_uses_custom_prompt_and_model() {
        let tmux = FakeTmuxOps::new();
        let home = PathBuf::from("/Users/jowi");
        let cfg = CreateConfig {
            dir: Some("/repo/axiom".to_string()),
            prompt: Some("/my-create".to_string()),
            prompt_file: None,
            model: Some("opus".to_string()),
        };

        launch_create(
            &crate::work::admission::AdmissionDeps::off(),
            None,
            &tmux,
            &cfg,
            &home,
            &test_identity(),
            RUNNER,
            crate::work::model::ModelDefaults::none(),
        )
        .unwrap();

        let command = tmux.calls().iter().find_map(|call| match call {
            TmuxCall::NewSessionWithCommand { command, .. } => Some(command.clone()),
            _ => None,
        });
        assert_eq!(
            command,
            Some("claude --model 'opus' '/my-create'".to_string())
        );
    }

    #[test]
    fn launch_create_uses_prompt_file_contents_as_the_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let prompt_path = dir.path().join("create-prompt.md");
        std::fs::write(&prompt_path, "/my-file-prompt").unwrap();
        let tmux = FakeTmuxOps::new();
        let home = PathBuf::from("/Users/jowi");
        let cfg = CreateConfig {
            dir: Some("/repo/axiom".to_string()),
            prompt: Some("/should-not-be-used".to_string()),
            prompt_file: Some(prompt_path.to_string_lossy().into_owned()),
            model: None,
        };

        launch_create(
            &crate::work::admission::AdmissionDeps::off(),
            None,
            &tmux,
            &cfg,
            &home,
            &test_identity(),
            RUNNER,
            crate::work::model::ModelDefaults::none(),
        )
        .unwrap();

        let command = tmux.calls().iter().find_map(|call| match call {
            TmuxCall::NewSessionWithCommand { command, .. } => Some(command.clone()),
            _ => None,
        });
        assert_eq!(
            command,
            Some("claude --model 'opus' '/my-file-prompt'".to_string())
        );
    }

    #[test]
    fn launch_create_errors_when_prompt_file_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let prompt_path = dir.path().join("missing-prompt.md");
        let tmux = FakeTmuxOps::new();
        let home = PathBuf::from("/Users/jowi");
        let cfg = CreateConfig {
            dir: Some("/repo/axiom".to_string()),
            prompt: None,
            prompt_file: Some(prompt_path.to_string_lossy().into_owned()),
            model: None,
        };

        let err = launch_create(
            &crate::work::admission::AdmissionDeps::off(),
            None,
            &tmux,
            &cfg,
            &home,
            &test_identity(),
            RUNNER,
            crate::work::model::ModelDefaults::none(),
        )
        .expect_err("missing prompt file should error");

        match &err {
            CreateLaunchError::PromptFile(inner) => {
                let message = inner.to_string();
                assert!(message.contains("[work.create].prompt_file"));
                assert!(message.contains(&prompt_path.to_string_lossy().to_string()));
            }
            other => panic!("expected PromptFile, got {other:?}"),
        }
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
