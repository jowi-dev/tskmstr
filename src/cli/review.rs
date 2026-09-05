//! `tm review fix <KEY>`: dispatch a Claude fix pass over the review
//! comments `vdiff` captured for a ticket's lane-run worktree, per
//! `docs/plans/board-vdiff-review-loop.md`.
//!
//! Steps, mirroring the plan's "`tm review fix <KEY>` — the dispatch
//! subcommand" section:
//!
//! 1. Resolve KEY's latest `kind = "lane"` run
//!    ([`crate::cli::runs::resolve_run`]) — that run's `worktree`/`branch`
//!    fields *are* the ticket's existing worktree and branch, no path
//!    reconstruction needed. No run, or a run with no recorded branch, is a
//!    typed error rather than a dispatch attempt.
//! 2. Export the worktree's captured review comments via
//!    [`crate::work::vdiff::VdiffOps::export_comments`].
//! 3. `vdiff --export-comments` exits `0` with the literal text `"No
//!    comments."` when the store is empty or absent — [`fix`] treats that as
//!    its own outcome ([`FixOutcome::NoComments`]) rather than dispatching a
//!    no-op run; no run row is created for it.
//! 4. Otherwise, wrap the exported markdown in fix-pass instructions
//!    ([`build_fix_prompt`]) and hand it to
//!    [`crate::work::run::prepare_review_fix`], which starts a tracked
//!    `kind = "review-fix"` run on the *existing* worktree/branch — no new
//!    worktree, no new branch.
//! 5. Dispatch, per [`Dispatch`] — the same three-way resolution
//!    [`crate::cli::work::run`] uses, and for the same reasons:
//!    - [`Dispatch::Interactive`] (the default) hosts the pass in a `fix`
//!      window of the ticket's `tm-<scope>-<key>` session, so it can be attached to
//!      and steered. A repeat pass becomes `fix-2`.
//!    - [`Dispatch::HeadlessForeground`] (`--fg`) runs
//!      [`crate::work::run::run_agent_and_finish`] synchronously.
//!    - [`Dispatch::Headless`] writes a
//!      [`crate::work::detach::SupervisorState`] and spawns the same `tm work
//!      __supervise` supervisor `tm work run`'s headless path uses — the
//!      supervisor only reads back a [`crate::work::run::PreparedRun`], so it
//!      has no notion of "lane" vs. "review-fix" runs at all.

use std::io::Write;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::agent::AgentRunner;
use crate::github::gh_cli::GhCli;
use crate::runs::{RunStore, RunStoreError};
use crate::work::detach::{DetachError, DetachSpawner, SupervisorState, supervisor_argv};
use crate::work::git::GitOps;
use crate::work::interactive::{
    FIX_WINDOW_NAME, InteractiveLaunchError, launch_interactive_run, resolve_action_window,
};
use crate::work::run::{
    Clock, PreparedRun, RunLaneError, RunLanePaths, prepare_review_fix, run_agent_and_finish,
    run_log_path,
};
use crate::work::runner::ProcessSpawner;
use crate::work::tmux::{TmuxError, TmuxOps};
use crate::work::vdiff::{VdiffError, VdiffOps};

use super::runs::{RunsCliError, resolve_run};
use super::work::Dispatch;

/// The literal text `vdiff --export-comments` prints (with exit `0`) when
/// the worktree's comment store is empty or absent. See [`VdiffOps::export_comments`]'s
/// doc comment for why this is a normal outcome, not an error.
const NO_COMMENTS: &str = "No comments.";

/// Errors surfaced by `tm review fix`.
#[derive(Debug, Error)]
pub enum ReviewCliError {
    /// Resolving KEY's latest lane run failed (no such run, or a `RunStore`
    /// error).
    #[error(transparent)]
    Runs(#[from] RunsCliError),

    /// KEY's latest lane run has no recorded branch. Every lane run started
    /// after branch tracking landed records one; a `None` here means the
    /// row predates that, or was started by a path that never recorded it —
    /// either way there's no branch to dispatch a fix pass onto.
    #[error(
        "the latest lane run for {ticket} has no recorded branch — nothing to dispatch a fix pass onto"
    )]
    MissingBranch {
        /// The ticket key that was looked up.
        ticket: String,
    },

    /// `vdiff --export-comments` failed (not found on `PATH`, couldn't be
    /// spawned, or exited nonzero) — distinct from [`FixOutcome::NoComments`],
    /// which is `vdiff` succeeding and reporting an empty store.
    #[error(transparent)]
    Vdiff(#[from] VdiffError),

    /// [`prepare_review_fix`] failed (a dirty worktree, a `git`/hooks/store
    /// failure).
    #[error(transparent)]
    Prepare(#[from] crate::work::run::ReviewFixError),

    /// The foreground spawn-wait-parse-finish tail
    /// ([`run_agent_and_finish`]) failed.
    #[error(transparent)]
    Run(#[from] RunLaneError),

    /// Spawning the detached supervisor failed.
    #[error(transparent)]
    Detach(#[from] DetachError),

    /// A `RunStore` operation outside [`prepare_review_fix`] (recording the
    /// detached run's log path) failed.
    #[error(transparent)]
    Store(#[from] RunStoreError),

    /// A filesystem/output-write operation failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// Serializing the detached supervisor's state file failed.
    #[error("failed to serialize run state: {0}")]
    Json(#[from] serde_json::Error),

    /// A `tmux` shell-out failed while resolving the interactive fix pass's
    /// window.
    #[error(transparent)]
    Tmux(#[from] TmuxError),

    /// Launching the interactive fix pass's tmux window failed — including
    /// the refusal to dispatch a second concurrent fix pass.
    #[error(transparent)]
    Interactive(#[from] InteractiveLaunchError),
}

/// The result of one `tm review fix` dispatch attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixOutcome {
    /// `vdiff --export-comments` reported an empty/absent comment store; no
    /// run was dispatched and no run row was created.
    NoComments,
    /// A run was dispatched (interactively or to a supervisor) or completed
    /// (`--fg`). `succeeded` mirrors [`crate::cli::work::run`]'s `Ok(bool)`
    /// convention: always `true` for a dispatch (there's nothing to report
    /// failed yet), `false` for an `--fg` run that finished but was recorded
    /// as failed.
    Dispatched {
        /// Whether the run succeeded (always `true` when not `--fg`).
        succeeded: bool,
    },
}

/// Dependencies [`fix`] needs beyond [`RunLanePaths`]: every trait-object
/// seam [`prepare_review_fix`]/[`run_agent_and_finish`]/the detached path
/// require, gathered the same way [`crate::cli::work::RunDeps`] gathers
/// `tm work run`'s dependencies.
pub struct ReviewFixDeps<'a> {
    /// Git operations (real or fake), for [`prepare_review_fix`]'s
    /// dirty-worktree check.
    pub git: &'a dyn GitOps,
    /// `gh` CLI operations (real or fake), for the post-run PR-URL lookup.
    pub gh: &'a dyn GhCli,
    /// Process spawning (real or fake).
    pub spawner: &'a dyn ProcessSpawner,
    /// The run-state store `start_run`/`finish_run` are called against.
    pub run_store: &'a RunStore,
    /// "Now" source for the run's timestamp.
    pub clock: &'a dyn Clock,
    /// `vdiff --export-comments` seam (real or fake).
    pub vdiff: &'a dyn VdiffOps,
    /// The invoking repo's backend identity; its
    /// [`session_slug`](crate::config::BackendIdentity::session_slug)
    /// qualifies the ticket's session name so same-numbered tickets in
    /// different repos never share a session (GitHub issue #10).
    pub backend_identity: &'a crate::config::BackendIdentity,
    /// Detached-supervisor process spawning (real or fake). Only used for
    /// [`Dispatch::Headless`].
    pub detach: &'a dyn DetachSpawner,
    /// This process's own executable path, re-exec'd as the detached
    /// supervisor. Only used for [`Dispatch::Headless`].
    pub current_exe: &'a Path,
    /// The run-state database path, threaded through to the detached
    /// supervisor's state file. Only used for [`Dispatch::Headless`].
    pub run_db_path: &'a Path,
    /// tmux operations (real or fake), for hosting an interactive fix pass
    /// in the ticket's session. Only used for [`Dispatch::Interactive`].
    pub tmux: &'a dyn TmuxOps,
    /// The AI coding agent this fix pass's invocation is built for, passed
    /// through to [`prepare_review_fix`]. See [`crate::agent::AgentRunner`]
    /// and GitHub issue #17.
    pub runner: &'a dyn AgentRunner,
}

/// Wrap `export` — the markdown [`VdiffOps::export_comments`] rendered,
/// grouped by file with `path:start-end` anchors — in instructions for the
/// fix-pass agent: address every comment, on the current branch, and commit
/// the result. The agent is deliberately told not to branch or re-worktree
/// itself: [`prepare_review_fix`] has already put it on the ticket's
/// existing branch, in its existing worktree, on purpose.
fn build_fix_prompt(export: &str) -> String {
    format!(
        "Address every review comment below on the current branch. Each \
         comment is anchored to a file and line range in the form \
         `path:start-end`. For each one, make the corresponding code \
         change and commit it — you are already on the branch these \
         comments were left against; do not create a new branch or \
         worktree.\n\n{export}"
    )
}

/// `tm review fix <KEY> [--headless] [--fg]`: see the module doc comment for
/// the full sequence.
///
/// # Errors
///
/// [`ReviewCliError::Runs`] if KEY has no `kind = "lane"` run;
/// [`ReviewCliError::MissingBranch`] if that run has no recorded branch;
/// [`ReviewCliError::Vdiff`] if `vdiff --export-comments` itself failed
/// (not `PATH`, spawn failure, or a nonzero exit — an empty store is
/// [`FixOutcome::NoComments`], not an error); [`ReviewCliError::Prepare`] if
/// [`prepare_review_fix`]'s preflight failed;
/// [`ReviewCliError::Interactive`] if a fix pass is already live in the
/// ticket's session.
pub fn fix(
    deps: &ReviewFixDeps<'_>,
    paths: &RunLanePaths,
    key: &str,
    dispatch: Dispatch,
    out: &mut dyn Write,
) -> Result<FixOutcome, ReviewCliError> {
    let scope = deps.backend_identity.scope();
    let run = resolve_run(deps.run_store, Some(&scope), key, Some("lane"))?;
    let branch = run
        .branch
        .clone()
        .ok_or_else(|| ReviewCliError::MissingBranch {
            ticket: run.ticket.clone(),
        })?;
    let worktree = PathBuf::from(&run.worktree);

    let export = deps.vdiff.export_comments(&worktree)?;
    if export.trim() == NO_COMMENTS {
        return Ok(FixOutcome::NoComments);
    }

    let prompt = build_fix_prompt(&export);
    let pid = match dispatch {
        Dispatch::HeadlessForeground => Some(std::process::id()),
        Dispatch::Headless | Dispatch::Interactive => None,
    };

    // Resolved before `prepare_review_fix` starts a run row, so a refusal to
    // run a second concurrent fix pass leaves nothing behind — see
    // `crate::work::interactive`'s module docs.
    //
    // Both tmux-windowed dispatches resolve a window here, and the refusal
    // applies to both: whether the live `fix` window hosts `claude` or only
    // tails its log, a fix pass for this ticket is in flight either way.
    // `--fg` gets no window at all — it has no log file to follow, and its
    // output is going to this very terminal.
    let target = match dispatch {
        Dispatch::Interactive | Dispatch::Headless => {
            let windows = deps.tmux.list_windows()?;
            Some(resolve_action_window(
                &windows,
                &deps.backend_identity.session_slug(),
                &run.ticket,
                FIX_WINDOW_NAME,
            )?)
        }
        Dispatch::HeadlessForeground => None,
    };

    let prepared: PreparedRun = prepare_review_fix(
        deps.git,
        deps.run_store,
        deps.clock,
        paths,
        &scope,
        &run.ticket,
        &run.lane,
        &worktree,
        &branch,
        prompt,
        pid,
        dispatch.run_mode(),
        deps.runner,
    )?;

    if dispatch == Dispatch::Interactive {
        let target = target
            .as_ref()
            .expect("an interactive dispatch always resolves a window");
        let prompt_path = paths.state_dir.join(format!(
            "{}-{}.prompt.md",
            prepared.wt_name, prepared.timestamp
        ));
        launch_interactive_run(deps.tmux, target, &prepared, &prompt_path, deps.runner)?;

        writeln!(out, "started   review-fix {} on {branch}", run.ticket)?;
        writeln!(out, "worktree  {}", worktree.display())?;
        writeln!(
            out,
            "window    {}:{}",
            target.session_name, target.window_name
        )?;
        writeln!(out, "attach:   tmux attach -t {}", target.session_name)?;
        writeln!(out, "watch:    tm runs watch")?;
        return Ok(FixOutcome::Dispatched { succeeded: true });
    }

    if dispatch == Dispatch::HeadlessForeground {
        let outcome = run_agent_and_finish(
            deps.spawner,
            deps.gh,
            deps.run_store,
            &prepared,
            deps.runner,
            out,
        )?;
        return Ok(FixOutcome::Dispatched {
            succeeded: !outcome.is_error,
        });
    }

    // Detached: mirrors crate::cli::work::run's detached branch exactly,
    // down to reusing the same "work __supervise" argv — see this module's
    // doc comment for why that supervisor is kind-agnostic.
    std::fs::create_dir_all(&paths.state_dir)?;
    let log_path = run_log_path(&paths.state_dir, &prepared.wt_name, &prepared.timestamp);
    deps.run_store
        .update_log_path(prepared.run_id, &log_path.to_string_lossy())?;

    let state_path = paths.state_dir.join(format!(
        "{}-{}.supervisor.json",
        prepared.wt_name, prepared.timestamp
    ));
    let state = SupervisorState {
        prepared: prepared.clone(),
        run_db_path: deps.run_db_path.to_path_buf(),
    };
    std::fs::write(&state_path, serde_json::to_string_pretty(&state)?)?;

    let argv = supervisor_argv(&state_path);
    deps.detach
        .spawn_detached(deps.current_exe, &argv, &worktree, &log_path)?;

    writeln!(out, "started   review-fix {} on {branch}", run.ticket)?;
    writeln!(out, "worktree  {}", worktree.display())?;
    writeln!(out, "log       {}", log_path.display())?;
    // After the spawn, deliberately: the spawn is what creates the log file
    // the viewer follows. A tmux failure here is reported, never returned —
    // the supervisor is already driving the pass (see `crate::work::viewer`).
    if let Some(target) = &target {
        crate::work::viewer::launch_and_report_viewer(
            deps.tmux,
            target,
            &worktree.to_string_lossy(),
            deps.current_exe,
            prepared.run_id,
            out,
        )?;
    }
    writeln!(out, "watch:    tm runs watch")?;

    Ok(FixOutcome::Dispatched { succeeded: true })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::claude::ClaudeRunner;
    use crate::github::gh_cli::FakeGhCli;
    use crate::runs::StartRun;
    use crate::work::detach::FakeDetachSpawner;
    use crate::work::git::FakeGitOps;
    use crate::work::run::FakeClock;
    use crate::work::runner::FakeProcessSpawner;
    use crate::work::tmux::{FakeTmuxOps, TmuxCall, TmuxWindow};
    use crate::work::vdiff::FakeVdiffOps;
    use tempfile::TempDir;

    fn setup() -> (TempDir, RunStore, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let worktree = tmp.path().join("Worktrees/axiom/proj-1");
        std::fs::create_dir_all(&worktree).unwrap();
        (tmp, run_store, worktree)
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

    fn seed_lane_run(run_store: &RunStore, worktree: &Path) {
        run_store
            .start_run(&StartRun {
                scope: String::new(),
                ticket: "PROJ-1".to_string(),
                lane: "mylane".to_string(),
                worktree: worktree.to_string_lossy().into_owned(),
                branch: Some("jowi-dev/proj-1-slug".to_string()),
                pid: None,
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();
    }

    fn paths(tmp: &TempDir) -> RunLanePaths {
        RunLanePaths {
            home: tmp.path().join("home"),
            state_dir: tmp.path().join("state"),
            hooks_deploy_dir: tmp.path().join("hooks"),
        }
    }

    fn canned_json() -> String {
        r#"{"session_id":"sess-1","is_error":false,"result":"fixed it"}"#.to_string()
    }

    #[test]
    fn fix_errors_when_the_ticket_has_no_lane_run() {
        let (tmp, run_store, _worktree) = setup();
        let git = FakeGitOps::new();
        let gh = FakeGhCli::new();
        let spawner = FakeProcessSpawner::success(canned_json());
        let clock = FakeClock((2026, 8, 18, 10, 0, 0));
        let vdiff = FakeVdiffOps::with_export(Ok("## file.rs\n\ncomment".to_string()));
        let detach = FakeDetachSpawner::new(4242);
        let tmux = FakeTmuxOps::new();
        let current_exe = PathBuf::from("/usr/local/bin/tm");
        let run_db_path = tmp.path().join("runs.db");
        let deps = ReviewFixDeps {
            git: &git,
            gh: &gh,
            spawner: &spawner,
            run_store: &run_store,
            clock: &clock,
            vdiff: &vdiff,
            detach: &detach,
            current_exe: &current_exe,
            run_db_path: &run_db_path,
            tmux: &tmux,
            backend_identity: test_identity(),
            runner: &ClaudeRunner,
        };
        let paths = paths(&tmp);
        let mut out = Vec::new();

        let err = fix(&deps, &paths, "PROJ-1", Dispatch::Headless, &mut out).unwrap_err();

        assert!(matches!(err, ReviewCliError::Runs(_)));
    }

    #[test]
    fn fix_errors_when_the_lane_run_has_no_branch() {
        let (tmp, run_store, worktree) = setup();
        run_store
            .start_run(&StartRun {
                scope: String::new(),
                ticket: "PROJ-1".to_string(),
                lane: "mylane".to_string(),
                worktree: worktree.to_string_lossy().into_owned(),
                branch: None,
                pid: None,
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();

        let git = FakeGitOps::new();
        let gh = FakeGhCli::new();
        let spawner = FakeProcessSpawner::success(canned_json());
        let clock = FakeClock((2026, 8, 18, 10, 0, 0));
        let vdiff = FakeVdiffOps::with_export(Ok("## file.rs\n\ncomment".to_string()));
        let detach = FakeDetachSpawner::new(4242);
        let tmux = FakeTmuxOps::new();
        let current_exe = PathBuf::from("/usr/local/bin/tm");
        let run_db_path = tmp.path().join("runs.db");
        let deps = ReviewFixDeps {
            git: &git,
            gh: &gh,
            spawner: &spawner,
            run_store: &run_store,
            clock: &clock,
            vdiff: &vdiff,
            detach: &detach,
            current_exe: &current_exe,
            run_db_path: &run_db_path,
            tmux: &tmux,
            backend_identity: test_identity(),
            runner: &ClaudeRunner,
        };
        let paths = paths(&tmp);
        let mut out = Vec::new();

        let err = fix(&deps, &paths, "PROJ-1", Dispatch::Headless, &mut out).unwrap_err();

        assert!(matches!(err, ReviewCliError::MissingBranch { ticket } if ticket == "PROJ-1"));
    }

    #[test]
    fn fix_reports_no_comments_and_creates_no_run_row() {
        let (tmp, run_store, worktree) = setup();
        seed_lane_run(&run_store, &worktree);

        let git = FakeGitOps::new();
        let gh = FakeGhCli::new();
        let spawner = FakeProcessSpawner::success(canned_json());
        let clock = FakeClock((2026, 8, 18, 10, 0, 0));
        let vdiff = FakeVdiffOps::with_export(Ok("No comments.".to_string()));
        let detach = FakeDetachSpawner::new(4242);
        let tmux = FakeTmuxOps::new();
        let current_exe = PathBuf::from("/usr/local/bin/tm");
        let run_db_path = tmp.path().join("runs.db");
        let deps = ReviewFixDeps {
            git: &git,
            gh: &gh,
            spawner: &spawner,
            run_store: &run_store,
            clock: &clock,
            vdiff: &vdiff,
            detach: &detach,
            current_exe: &current_exe,
            run_db_path: &run_db_path,
            tmux: &tmux,
            backend_identity: test_identity(),
            runner: &ClaudeRunner,
        };
        let paths = paths(&tmp);
        let mut out = Vec::new();

        let outcome = fix(&deps, &paths, "PROJ-1", Dispatch::Headless, &mut out).unwrap();

        assert_eq!(outcome, FixOutcome::NoComments);
        assert_eq!(run_store.list_runs().unwrap().len(), 1); // only the seeded lane run
        assert!(detach.recorded.lock().unwrap().is_empty());
    }

    #[test]
    fn fix_surfaces_vdiff_not_found_distinctly() {
        let (tmp, run_store, worktree) = setup();
        seed_lane_run(&run_store, &worktree);

        let git = FakeGitOps::new();
        let gh = FakeGhCli::new();
        let spawner = FakeProcessSpawner::success(canned_json());
        let clock = FakeClock((2026, 8, 18, 10, 0, 0));
        let vdiff = FakeVdiffOps::with_export(Err(VdiffError::NotFound));
        let detach = FakeDetachSpawner::new(4242);
        let tmux = FakeTmuxOps::new();
        let current_exe = PathBuf::from("/usr/local/bin/tm");
        let run_db_path = tmp.path().join("runs.db");
        let deps = ReviewFixDeps {
            git: &git,
            gh: &gh,
            spawner: &spawner,
            run_store: &run_store,
            clock: &clock,
            vdiff: &vdiff,
            detach: &detach,
            current_exe: &current_exe,
            run_db_path: &run_db_path,
            tmux: &tmux,
            backend_identity: test_identity(),
            runner: &ClaudeRunner,
        };
        let paths = paths(&tmp);
        let mut out = Vec::new();

        let err = fix(&deps, &paths, "PROJ-1", Dispatch::Headless, &mut out).unwrap_err();

        assert!(matches!(err, ReviewCliError::Vdiff(VdiffError::NotFound)));
        assert_eq!(run_store.list_runs().unwrap().len(), 1);
    }

    #[test]
    fn fix_detached_dispatches_a_review_fix_run_via_the_work_supervisor() {
        let (tmp, run_store, worktree) = setup();
        seed_lane_run(&run_store, &worktree);

        let git = FakeGitOps::new();
        let gh = FakeGhCli::new();
        let spawner = FakeProcessSpawner::success(canned_json());
        let clock = FakeClock((2026, 8, 18, 10, 0, 0));
        let vdiff = FakeVdiffOps::with_export(Ok(
            "## src/foo.rs\n\nsrc/foo.rs:10-12 nit: rename this".to_string(),
        ));
        let detach = FakeDetachSpawner::new(4242);
        let tmux = FakeTmuxOps::new();
        let current_exe = PathBuf::from("/usr/local/bin/tm");
        let run_db_path = tmp.path().join("runs.db");
        let deps = ReviewFixDeps {
            git: &git,
            gh: &gh,
            spawner: &spawner,
            run_store: &run_store,
            clock: &clock,
            vdiff: &vdiff,
            detach: &detach,
            current_exe: &current_exe,
            run_db_path: &run_db_path,
            tmux: &tmux,
            backend_identity: test_identity(),
            runner: &ClaudeRunner,
        };
        let paths = paths(&tmp);
        let mut out = Vec::new();

        let outcome = fix(&deps, &paths, "PROJ-1", Dispatch::Headless, &mut out).unwrap();

        assert_eq!(outcome, FixOutcome::Dispatched { succeeded: true });
        // No claude process was spawned in this (short-lived, foreground)
        // process -- that's the supervisor's job.
        assert!(spawner.recorded.lock().unwrap().is_empty());

        let recorded = detach.recorded.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].program, current_exe);
        assert_eq!(recorded[0].argv[0], "work");
        assert_eq!(recorded[0].argv[1], "__supervise");
        assert_eq!(recorded[0].working_dir, worktree);

        let review_fix_run = run_store
            .latest_run_for_ticket_kind(None, "PROJ-1", Some("review-fix"))
            .unwrap()
            .unwrap();
        assert_eq!(review_fix_run.ticket, "PROJ-1");
        assert_eq!(
            review_fix_run.branch,
            Some("jowi-dev/proj-1-slug".to_string())
        );
        assert!(review_fix_run.log_path.is_some());

        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("started   review-fix PROJ-1 on jowi-dev/proj-1-slug"));
    }

    /// Issue #2 phase 4: like `tm work run --headless`, a headless fix pass
    /// gets a *viewer* window over its log, never the supervisor's `claude`.
    #[test]
    fn fix_detached_gets_a_viewer_window_over_its_log() {
        let (tmp, run_store, worktree) = setup();
        seed_lane_run(&run_store, &worktree);

        let git = FakeGitOps::new();
        let gh = FakeGhCli::new();
        let spawner = FakeProcessSpawner::success(canned_json());
        let clock = FakeClock((2026, 8, 18, 10, 0, 0));
        let vdiff = FakeVdiffOps::with_export(Ok(
            "## src/foo.rs\n\nsrc/foo.rs:10-12 nit: rename this".to_string(),
        ));
        let detach = FakeDetachSpawner::new(4242);
        let tmux = FakeTmuxOps::new();
        let current_exe = PathBuf::from("/usr/local/bin/tm");
        let run_db_path = tmp.path().join("runs.db");
        let deps = ReviewFixDeps {
            git: &git,
            gh: &gh,
            spawner: &spawner,
            run_store: &run_store,
            clock: &clock,
            vdiff: &vdiff,
            detach: &detach,
            current_exe: &current_exe,
            run_db_path: &run_db_path,
            tmux: &tmux,
            backend_identity: test_identity(),
            runner: &ClaudeRunner,
        };
        let paths = paths(&tmp);
        let mut out = Vec::new();

        fix(&deps, &paths, "PROJ-1", Dispatch::Headless, &mut out).unwrap();

        let review_fix_run = run_store
            .latest_run_for_ticket_kind(None, "PROJ-1", Some("review-fix"))
            .unwrap()
            .unwrap();
        let (window_name, env, command) = tmux
            .calls()
            .iter()
            .find_map(|call| match call {
                TmuxCall::NewSessionWithCommand {
                    name,
                    window_name,
                    env,
                    command,
                    ..
                } => {
                    assert_eq!(name, "tm-proj-proj-1");
                    Some((window_name.clone(), env.clone(), command.clone()))
                }
                _ => None,
            })
            .expect("a viewer window was launched");

        assert_eq!(window_name, "fix");
        assert_eq!(
            command,
            format!(
                "'/usr/local/bin/tm' runs logs {} --follow",
                review_fix_run.id
            )
        );
        assert!(env.is_empty(), "a viewer owns no run: {env:?}");

        let printed = String::from_utf8(out).unwrap();
        assert!(
            printed.contains("window    tm-proj-proj-1:fix (log viewer)"),
            "{printed}"
        );
    }

    #[test]
    fn fix_interactive_hosts_the_pass_in_the_tickets_tmux_session() {
        let (tmp, run_store, worktree) = setup();
        seed_lane_run(&run_store, &worktree);

        let git = FakeGitOps::new();
        let gh = FakeGhCli::new();
        let spawner = FakeProcessSpawner::success(canned_json());
        let clock = FakeClock((2026, 8, 18, 10, 0, 0));
        let vdiff = FakeVdiffOps::with_export(Ok(
            "## src/foo.rs\n\nsrc/foo.rs:10-12 nit: rename this".to_string(),
        ));
        let detach = FakeDetachSpawner::new(4242);
        // A dead `fix` window from the previous pass, so this one has to
        // suffix past it.
        let tmux = FakeTmuxOps::new().with_list_windows(Ok(vec![TmuxWindow {
            session: "tm-proj-proj-1".to_string(),
            name: "fix".to_string(),
            dead: true,
        }]));
        let current_exe = PathBuf::from("/usr/local/bin/tm");
        let run_db_path = tmp.path().join("runs.db");
        let deps = ReviewFixDeps {
            git: &git,
            gh: &gh,
            spawner: &spawner,
            run_store: &run_store,
            clock: &clock,
            vdiff: &vdiff,
            detach: &detach,
            current_exe: &current_exe,
            run_db_path: &run_db_path,
            tmux: &tmux,
            backend_identity: test_identity(),
            runner: &ClaudeRunner,
        };
        let paths = paths(&tmp);
        let mut out = Vec::new();

        let outcome = fix(&deps, &paths, "PROJ-1", Dispatch::Interactive, &mut out).unwrap();

        assert_eq!(outcome, FixOutcome::Dispatched { succeeded: true });
        assert!(detach.recorded.lock().unwrap().is_empty());
        assert!(spawner.recorded.lock().unwrap().is_empty());

        let review_fix_run = run_store
            .latest_run_for_ticket_kind(None, "PROJ-1", Some("review-fix"))
            .unwrap()
            .unwrap();

        let (window_name, env) = tmux
            .calls()
            .iter()
            .find_map(|call| match call {
                TmuxCall::NewWindowWithCommand {
                    window_name, env, ..
                } => Some((window_name.clone(), env.clone())),
                _ => None,
            })
            .expect("a fix window was appended to the ticket's session");
        assert_eq!(window_name, "fix-2");
        assert_eq!(
            env,
            vec![(
                "TSKMSTR_SESSION_RUN_ID".to_string(),
                review_fix_run.id.to_string()
            )]
        );

        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("window    tm-proj-proj-1:fix-2"));
    }

    #[test]
    fn fix_interactive_refuses_while_a_fix_pass_is_live_and_creates_no_run_row() {
        let (tmp, run_store, worktree) = setup();
        seed_lane_run(&run_store, &worktree);

        let git = FakeGitOps::new();
        let gh = FakeGhCli::new();
        let spawner = FakeProcessSpawner::success(canned_json());
        let clock = FakeClock((2026, 8, 18, 10, 0, 0));
        let vdiff = FakeVdiffOps::with_export(Ok("## src/foo.rs\n\ncomment".to_string()));
        let detach = FakeDetachSpawner::new(4242);
        let tmux = FakeTmuxOps::new().with_list_windows(Ok(vec![TmuxWindow {
            session: "tm-proj-proj-1".to_string(),
            name: "fix".to_string(),
            dead: false,
        }]));
        let current_exe = PathBuf::from("/usr/local/bin/tm");
        let run_db_path = tmp.path().join("runs.db");
        let deps = ReviewFixDeps {
            git: &git,
            gh: &gh,
            spawner: &spawner,
            run_store: &run_store,
            clock: &clock,
            vdiff: &vdiff,
            detach: &detach,
            current_exe: &current_exe,
            run_db_path: &run_db_path,
            tmux: &tmux,
            backend_identity: test_identity(),
            runner: &ClaudeRunner,
        };
        let paths = paths(&tmp);
        let mut out = Vec::new();

        let err = fix(&deps, &paths, "PROJ-1", Dispatch::Interactive, &mut out).unwrap_err();

        assert!(matches!(err, ReviewCliError::Interactive(_)));
        // Only the seeded lane run: two concurrent fix passes on one
        // worktree would fight over the same files.
        assert_eq!(run_store.list_runs().unwrap().len(), 1);
    }

    #[test]
    fn fix_foreground_runs_claude_synchronously_and_finishes_the_run() {
        let (tmp, run_store, worktree) = setup();
        seed_lane_run(&run_store, &worktree);

        let git = FakeGitOps::new();
        let gh = FakeGhCli::new();
        let spawner = FakeProcessSpawner::success(canned_json());
        let clock = FakeClock((2026, 8, 18, 10, 0, 0));
        let vdiff = FakeVdiffOps::with_export(Ok(
            "## src/foo.rs\n\nsrc/foo.rs:10-12 nit: rename this".to_string(),
        ));
        let detach = FakeDetachSpawner::new(4242);
        let tmux = FakeTmuxOps::new();
        let current_exe = PathBuf::from("/usr/local/bin/tm");
        let run_db_path = tmp.path().join("runs.db");
        let deps = ReviewFixDeps {
            git: &git,
            gh: &gh,
            spawner: &spawner,
            run_store: &run_store,
            clock: &clock,
            vdiff: &vdiff,
            detach: &detach,
            current_exe: &current_exe,
            run_db_path: &run_db_path,
            tmux: &tmux,
            backend_identity: test_identity(),
            runner: &ClaudeRunner,
        };
        let paths = paths(&tmp);
        let mut out = Vec::new();

        let outcome = fix(
            &deps,
            &paths,
            "PROJ-1",
            Dispatch::HeadlessForeground,
            &mut out,
        )
        .unwrap();

        assert_eq!(outcome, FixOutcome::Dispatched { succeeded: true });
        assert!(detach.recorded.lock().unwrap().is_empty());

        let recorded = spawner.recorded.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert!(recorded[0].current_dir.ends_with("proj-1"));
        assert!(
            recorded[0]
                .args
                .iter()
                .any(|arg| arg.contains("Address every review comment"))
        );

        let review_fix_run = run_store
            .latest_run_for_ticket_kind(None, "PROJ-1", Some("review-fix"))
            .unwrap()
            .unwrap();
        assert_eq!(review_fix_run.status, crate::runs::RunStatus::Done);
    }

    #[test]
    fn fix_foreground_reports_failure_without_erroring() {
        let (tmp, run_store, worktree) = setup();
        seed_lane_run(&run_store, &worktree);

        let git = FakeGitOps::new();
        let gh = FakeGhCli::new();
        let spawner = FakeProcessSpawner::with_exit_code(canned_json(), 1);
        let clock = FakeClock((2026, 8, 18, 10, 0, 0));
        let vdiff = FakeVdiffOps::with_export(Ok("## src/foo.rs\n\ncomment".to_string()));
        let detach = FakeDetachSpawner::new(4242);
        let tmux = FakeTmuxOps::new();
        let current_exe = PathBuf::from("/usr/local/bin/tm");
        let run_db_path = tmp.path().join("runs.db");
        let deps = ReviewFixDeps {
            git: &git,
            gh: &gh,
            spawner: &spawner,
            run_store: &run_store,
            clock: &clock,
            vdiff: &vdiff,
            detach: &detach,
            current_exe: &current_exe,
            run_db_path: &run_db_path,
            tmux: &tmux,
            backend_identity: test_identity(),
            runner: &ClaudeRunner,
        };
        let paths = paths(&tmp);
        let mut out = Vec::new();

        let outcome = fix(
            &deps,
            &paths,
            "PROJ-1",
            Dispatch::HeadlessForeground,
            &mut out,
        )
        .unwrap();

        assert_eq!(outcome, FixOutcome::Dispatched { succeeded: false });
    }

    #[test]
    fn fix_refuses_a_dirty_worktree_without_dispatching() {
        let (tmp, run_store, worktree) = setup();
        seed_lane_run(&run_store, &worktree);

        let git = FakeGitOps::new().with_status_is_clean(Ok(false));
        let gh = FakeGhCli::new();
        let spawner = FakeProcessSpawner::success(canned_json());
        let clock = FakeClock((2026, 8, 18, 10, 0, 0));
        let vdiff = FakeVdiffOps::with_export(Ok("## src/foo.rs\n\ncomment".to_string()));
        let detach = FakeDetachSpawner::new(4242);
        let tmux = FakeTmuxOps::new();
        let current_exe = PathBuf::from("/usr/local/bin/tm");
        let run_db_path = tmp.path().join("runs.db");
        let deps = ReviewFixDeps {
            git: &git,
            gh: &gh,
            spawner: &spawner,
            run_store: &run_store,
            clock: &clock,
            vdiff: &vdiff,
            detach: &detach,
            current_exe: &current_exe,
            run_db_path: &run_db_path,
            tmux: &tmux,
            backend_identity: test_identity(),
            runner: &ClaudeRunner,
        };
        let paths = paths(&tmp);
        let mut out = Vec::new();

        let err = fix(&deps, &paths, "PROJ-1", Dispatch::Headless, &mut out).unwrap_err();

        assert!(matches!(err, ReviewCliError::Prepare(_)));
        assert!(detach.recorded.lock().unwrap().is_empty());
        // Only the seeded lane run -- no review-fix row was created.
        assert_eq!(run_store.list_runs().unwrap().len(), 1);
    }
}
