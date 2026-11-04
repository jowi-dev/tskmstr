//! The session `tm ticket retro --defect` starts to save the defect's lesson
//! as a thatch project memory (GitHub issue #227).
//!
//! Recording a retro verdict only writes runs.db, where no future agent
//! looks. This session closes that gap: tm hands a headless agent the
//! ticket, its PR, the severity, and the note, and the agent works out what
//! in the shipped change caused the defect and saves one project memory
//! naming the area, the cause, and the warning sign (see
//! [`AgentRunner::default_defect_retro_prompt_template`]). Per ADR-0011, tm
//! never writes the memory itself: thatch's tools do, from inside the
//! session.
//!
//! The launch mirrors `tm merge`'s memory-consolidation session: admitted,
//! headless, untracked (no run row), opened in a tmux window in the repo
//! checkout so thatch finds the project store from the repo's remote, with
//! output going to a log under the state directory. Unlike that session it
//! needs a shell and file reads for its archaeology, so it runs with
//! [`ToolScope::Full`](crate::agent::ToolScope::Full) and the runner's
//! default permission mode.

use std::io::Write;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::agent::{AgentRunner, InvocationInputs, RunMode};
use crate::config::{BackendIdentity, RetroConfig};
use crate::runs::{RetroSeverity, RunStore};
use crate::work::merge::{headless_command_line, open_command_window};
use crate::work::tmux::{TmuxError, TmuxOps};

/// Base name (before `unique_window_name` suffixing) of the tmux window the
/// defect retro session runs in.
pub const RETRO_WINDOW_BASE: &str = "retro";

/// The run kind the defect retro session is admitted under. No run row is
/// recorded for it, as for `tm merge`'s memory session: the kind only keys
/// admission's memory estimate.
pub const RETRO_KIND: &str = "retro-memory";

/// Turn budget for the defect retro session: room to read a PR diff and the
/// code around it, save one memory, and file one bug, while staying small
/// because nothing tracks or reaps the session.
pub const RETRO_MAX_TURNS: &str = "60";

/// The `{pr}` text when runs.db has no PR URL for the ticket.
const NO_PR_RECORDED: &str =
    "none recorded by tm; find it with `gh pr list --state merged --search <ticket>`";

/// One recorded `--defect` retro, as the session's prompt describes it.
#[derive(Debug, Clone, Copy)]
pub struct DefectRetro<'a> {
    /// The normalized ticket key.
    pub key: &'a str,
    /// The recorded severity.
    pub severity: RetroSeverity,
    /// The retro's note, if one was given.
    pub note: Option<&'a str>,
    /// The ticket's PR URL, from [`ticket_pr_url`].
    pub pr_url: Option<&'a str>,
}

/// Everything [`start_defect_session`] needs.
pub struct RetroSessionDeps<'a> {
    /// Opens the session's window.
    pub tmux: &'a dyn TmuxOps,
    /// The configured agent runner.
    pub runner: &'a dyn AgentRunner,
    /// The runs DB, for admission's running total. `None` admits without
    /// one.
    pub run_store: Option<&'a RunStore>,
    /// Qualifies the ticket session name outside tmux.
    pub identity: &'a BackendIdentity,
    /// Memory and usage admission.
    pub admission: crate::work::admission::AdmissionDeps<'a>,
    /// The model chain below `[work.retro] model`.
    pub model_defaults: crate::work::model::ModelDefaults<'a>,
    /// The merged `[work.retro]` settings.
    pub retro_cfg: &'a RetroConfig,
    /// Timestamps the log file name.
    pub clock: &'a dyn crate::work::review_watch::Clock,
    /// Where the session's log is written.
    pub state_dir: &'a Path,
    /// The repo checkout the session runs in.
    pub repo_root: &'a Path,
}

/// Errors returned by [`start_defect_session`]. The caller reports them as
/// a warning: the retro verdict is already recorded by then.
#[derive(Debug, Error)]
pub enum RetroSessionError {
    /// Memory admission refused the session.
    #[error(transparent)]
    Admission(#[from] crate::work::admission::AdmissionError),

    /// Opening the tmux window failed.
    #[error(transparent)]
    Tmux(#[from] TmuxError),

    /// The repo checkout to run the session in could not be found, e.g.
    /// `tm ticket retro` ran outside a git repository.
    #[error(transparent)]
    Git(#[from] crate::work::git::GitError),

    /// `state_dir` could not be created to hold the session's log.
    #[error("failed to create {path}: {source}", path = path.display())]
    StateDir {
        /// The directory that could not be created.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },

    /// Writing the status line failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// The most recent PR URL runs.db recorded for `key` in `scope`, or `None`
/// when no run of the ticket recorded one (or the lookup failed: the
/// prompt then tells the agent to find the PR itself).
pub fn ticket_pr_url(store: &RunStore, scope: &str, key: &str) -> Option<String> {
    store
        .runs_for_ticket(Some(scope), key)
        .ok()?
        .into_iter()
        .rev()
        .find_map(|run| run.pr_url)
}

/// The runner's defect retro prompt with `retro` substituted in. A missing
/// PR URL becomes a line telling the agent how to find the PR, and a
/// missing note becomes `none`.
pub fn defect_retro_prompt(runner: &dyn AgentRunner, retro: &DefectRetro<'_>) -> String {
    runner
        .default_defect_retro_prompt_template()
        .replace("{key}", retro.key)
        .replace("{severity}", retro.severity.as_str())
        .replace("{pr}", retro.pr_url.unwrap_or(NO_PR_RECORDED))
        .replace("{note}", retro.note.unwrap_or("none"))
}

/// Starts the defect retro session for `retro`: admits it under
/// [`RETRO_KIND`], builds a [`RunMode::Headless`] invocation of
/// [`defect_retro_prompt`] with `[work.retro] model` ahead of the rest of
/// the model chain and a [`RETRO_MAX_TURNS`] budget, and opens it in a
/// [`RETRO_WINDOW_BASE`] window in `repo_root`: in the current tmux session
/// when there is one, otherwise in the ticket's own session. Output goes to
/// `retro-<key>-<timestamp>.log` under `state_dir`, and a status line
/// naming the window and log is written to `out`.
pub fn start_defect_session(
    deps: &RetroSessionDeps<'_>,
    retro: &DefectRetro<'_>,
    out: &mut dyn Write,
) -> Result<(), RetroSessionError> {
    let repo = crate::work::run::dir_repo_key(deps.repo_root);
    let admitted = deps.admission.admit_tracked(
        deps.run_store,
        &crate::work::admission::LaunchRequest {
            kind: RETRO_KIND,
            agent: deps.runner.name(),
            repo: &repo,
            own_run: None,
        },
        crate::work::admission::LaunchOrigin::Human,
        out,
    )?;
    let model = deps
        .model_defaults
        .resolve(
            deps.runner,
            &[crate::work::model::ModelCandidate {
                source: "[work.retro] model",
                model: deps.retro_cfg.model.as_deref(),
            }],
        )
        .model;
    let invocation = deps.runner.build_invocation(
        &admitted,
        InvocationInputs {
            prompt: defect_retro_prompt(deps.runner, retro),
            model,
            max_turns: Some(RETRO_MAX_TURNS.to_string()),
            permission_mode: None,
            settings_path: None,
            run_id: None,
            mode: RunMode::Headless,
            tool_scope: crate::agent::ToolScope::Full,
        },
    );

    std::fs::create_dir_all(deps.state_dir).map_err(|source| RetroSessionError::StateDir {
        path: deps.state_dir.to_path_buf(),
        source,
    })?;
    let log_path = deps.state_dir.join(format!(
        "retro-{}-{}.log",
        retro.key.to_lowercase(),
        deps.clock.now_unix_secs()
    ));
    let command = headless_command_line(&invocation, &log_path);
    let opened = open_command_window(
        deps.tmux,
        deps.identity,
        retro.key,
        RETRO_WINDOW_BASE,
        deps.repo_root,
        &invocation.env_set,
        &command,
    )?;

    writeln!(
        out,
        "saving the defect lesson for {} as a thatch memory (window {}:{}, log {})",
        retro.key,
        opened.target,
        opened.window,
        log_path.display()
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::claude::ClaudeRunner;
    use crate::config::{ModelsConfig, RetroConfig};
    use crate::runs::{FinishRun, RunStatus, StartRun};
    use crate::work::naming::ticket_session_name;
    use crate::work::review_watch::FakeClock;
    use crate::work::tmux::{FakeTmuxOps, TmuxCall};
    use tempfile::tempdir;

    fn retro<'a>(pr_url: Option<&'a str>, note: Option<&'a str>) -> DefectRetro<'a> {
        DefectRetro {
            key: "PROJ-7",
            severity: RetroSeverity::Major,
            note,
            pr_url,
        }
    }

    /// Every window opened: `(session, window, dir, command)`.
    fn windows(tmux: &FakeTmuxOps) -> Vec<(String, String, String, String)> {
        tmux.calls()
            .into_iter()
            .filter_map(|call| match call {
                TmuxCall::NewSessionWithCommand {
                    name,
                    dir,
                    window_name,
                    command,
                    ..
                }
                | TmuxCall::NewWindowWithCommand {
                    name,
                    dir,
                    window_name,
                    command,
                    ..
                } => Some((name, window_name, dir, command)),
                _ => None,
            })
            .collect()
    }

    struct Fixture {
        tmux: FakeTmuxOps,
        identity: BackendIdentity,
        models: ModelsConfig,
        retro_cfg: RetroConfig,
        clock: FakeClock,
        state_dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Fixture {
                tmux: FakeTmuxOps::new(),
                identity: BackendIdentity::Jira {
                    base_url: "https://example.atlassian.net".to_string(),
                    project_key: "PROJ".to_string(),
                },
                models: ModelsConfig::UNSET,
                retro_cfg: RetroConfig::default(),
                clock: FakeClock::at(1_700_000_000),
                state_dir: tempdir().unwrap(),
            }
        }

        fn deps(&self) -> RetroSessionDeps<'_> {
            RetroSessionDeps {
                tmux: &self.tmux,
                runner: &ClaudeRunner,
                run_store: None,
                identity: &self.identity,
                admission: crate::work::admission::AdmissionDeps::off(),
                model_defaults: crate::work::model::ModelDefaults {
                    work_default: None,
                    models: &self.models,
                },
                retro_cfg: &self.retro_cfg,
                clock: &self.clock,
                state_dir: self.state_dir.path(),
                repo_root: Path::new("/repo"),
            }
        }
    }

    #[test]
    fn prompt_names_the_ticket_severity_pr_and_note() {
        let prompt = defect_retro_prompt(
            &ClaudeRunner,
            &retro(
                Some("https://github.com/me/proj/pull/9"),
                Some("logins failed"),
            ),
        );
        assert!(prompt.contains("Ticket PROJ-7"), "{prompt}");
        assert!(prompt.contains("Severity: major"), "{prompt}");
        assert!(
            prompt.contains("Pull request: https://github.com/me/proj/pull/9"),
            "{prompt}"
        );
        assert!(prompt.contains("note: logins failed"), "{prompt}");
        assert!(
            !prompt.contains('{'),
            "every placeholder substituted: {prompt}"
        );
    }

    #[test]
    fn prompt_without_pr_or_note_says_how_to_find_the_pr() {
        let prompt = defect_retro_prompt(&ClaudeRunner, &retro(None, None));
        assert!(
            prompt.contains("Pull request: none recorded by tm; find it with `gh pr list"),
            "{prompt}"
        );
        assert!(prompt.contains("note: none"), "{prompt}");
    }

    #[test]
    fn ticket_pr_url_is_the_latest_recorded_pr() {
        let dir = tempdir().unwrap();
        let store = RunStore::open(&dir.path().join("runs.db")).unwrap();
        let start = |ticket: &str| {
            store
                .start_run(&StartRun {
                    scope: "scope-a".to_string(),
                    ticket: ticket.to_string(),
                    lane: "backend".to_string(),
                    worktree: "/tmp/wt".to_string(),
                    branch: None,
                    pid: None,
                    kind: "lane".to_string(),
                    log_path: None,
                })
                .unwrap()
        };
        let finish = |id: i64, pr: Option<&str>| {
            store
                .finish_run(
                    id,
                    &FinishRun {
                        status: RunStatus::Done,
                        pr_url: pr.map(str::to_string),
                        ..Default::default()
                    },
                )
                .unwrap()
        };

        assert_eq!(ticket_pr_url(&store, "scope-a", "PROJ-7"), None);

        let first = start("PROJ-7");
        finish(first, Some("https://github.com/me/proj/pull/1"));
        let second = start("PROJ-7");
        finish(second, Some("https://github.com/me/proj/pull/2"));
        let third = start("PROJ-7");
        finish(third, None);

        assert_eq!(
            ticket_pr_url(&store, "scope-a", "PROJ-7").as_deref(),
            Some("https://github.com/me/proj/pull/2")
        );
    }

    #[test]
    fn starts_a_headless_session_in_the_repo_logging_to_the_state_dir() {
        let fx = Fixture::new();
        let mut out = Vec::new();

        start_defect_session(&fx.deps(), &retro(None, None), &mut out).expect("started");

        let opened = windows(&fx.tmux);
        assert_eq!(opened.len(), 1, "{opened:?}");
        let (session, window, dir, command) = &opened[0];
        assert_eq!(
            session,
            &ticket_session_name(&fx.identity.session_slug(), "PROJ-7"),
            "outside tmux the session gets its own ticket session"
        );
        assert_eq!(window, RETRO_WINDOW_BASE);
        assert_eq!(dir, "/repo");
        assert!(command.contains("'-p'"), "headless: {command}");
        assert!(
            command.contains(&format!("'--max-turns' '{RETRO_MAX_TURNS}'")),
            "{command}"
        );
        assert!(
            !command.contains("mcp__thatch"),
            "archaeology needs more than thatch's tools: {command}"
        );
        let log = fx.state_dir.path().join("retro-proj-7-1700000000.log");
        assert!(command.contains(&*log.to_string_lossy()), "{command}");

        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("saving the defect lesson for PROJ-7"), "{out}");
    }

    #[test]
    fn opens_in_the_current_tmux_session_when_there_is_one() {
        let mut fx = Fixture::new();
        fx.tmux = FakeTmuxOps::new().with_current_session_name(Ok(Some("mine".to_string())));

        start_defect_session(&fx.deps(), &retro(None, None), &mut Vec::new()).expect("started");

        let opened = windows(&fx.tmux);
        assert_eq!(opened[0].0, "mine");
    }

    #[test]
    fn retro_model_is_passed_to_the_agent() {
        let mut fx = Fixture::new();
        fx.retro_cfg.model = Some("claude-retro-model".to_string());

        start_defect_session(&fx.deps(), &retro(None, None), &mut Vec::new()).expect("started");

        let opened = windows(&fx.tmux);
        assert!(
            opened[0].3.contains("'--model' 'claude-retro-model'"),
            "{}",
            opened[0].3
        );
    }

    #[test]
    fn a_tmux_failure_is_an_error() {
        let mut fx = Fixture::new();
        fx.tmux = FakeTmuxOps::new().with_current_session_name(Err(TmuxError::Command {
            command: "tmux display-message".to_string(),
            exit_code: Some(1),
            stderr: "boom".to_string(),
        }));

        let err = start_defect_session(&fx.deps(), &retro(None, None), &mut Vec::new())
            .expect_err("tmux failed");
        assert!(matches!(err, RetroSessionError::Tmux(_)), "{err:?}");
    }
}
