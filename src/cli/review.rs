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
//! 5. Dispatch: `--fg` runs [`crate::work::run::run_claude_and_finish`]
//!    synchronously, mirroring [`crate::cli::work::run`]'s `fg` branch;
//!    otherwise this writes a [`crate::work::detach::SupervisorState`] and
//!    spawns the same `tm work __supervise` supervisor `tm work run`'s
//!    detached path uses — the supervisor only reads back a
//!    [`crate::work::run::PreparedRun`], so it has no notion of "lane" vs.
//!    "review-fix" runs at all.

use std::io::Write;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::github::gh_cli::GhCli;
use crate::runs::{RunStore, RunStoreError};
use crate::work::detach::{DetachError, DetachSpawner, SupervisorState, supervisor_argv};
use crate::work::git::GitOps;
use crate::work::run::{
    Clock, PreparedRun, RunLaneError, RunLanePaths, prepare_review_fix, run_claude_and_finish,
    run_log_path,
};
use crate::work::runner::ProcessSpawner;
use crate::work::vdiff::{VdiffError, VdiffOps};

use super::runs::{RunsCliError, resolve_run};

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
    /// ([`run_claude_and_finish`]) failed.
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
}

/// The result of one `tm review fix` dispatch attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixOutcome {
    /// `vdiff --export-comments` reported an empty/absent comment store; no
    /// run was dispatched and no run row was created.
    NoComments,
    /// A run was dispatched (detached) or completed (`--fg`). `succeeded`
    /// mirrors [`crate::cli::work::run`]'s `Ok(bool)` convention: always
    /// `true` for a detached dispatch (there's nothing to report failed
    /// yet), `false` for an `--fg` run that finished but was recorded as
    /// failed.
    Dispatched {
        /// Whether the run succeeded (always `true` when detached).
        succeeded: bool,
    },
}

/// Dependencies [`fix`] needs beyond [`RunLanePaths`]: every trait-object
/// seam [`prepare_review_fix`]/[`run_claude_and_finish`]/the detached path
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
    /// Detached-supervisor process spawning (real or fake). Only used when
    /// `fix`'s `fg` argument is `false`.
    pub detach: &'a dyn DetachSpawner,
    /// This process's own executable path, re-exec'd as the detached
    /// supervisor. Only used when `fg` is `false`.
    pub current_exe: &'a Path,
    /// The run-state database path, threaded through to the detached
    /// supervisor's state file. Only used when `fg` is `false`.
    pub run_db_path: &'a Path,
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

/// `tm review fix <KEY> [--fg]`: see the module doc comment for the full
/// sequence.
///
/// # Errors
///
/// [`ReviewCliError::Runs`] if KEY has no `kind = "lane"` run;
/// [`ReviewCliError::MissingBranch`] if that run has no recorded branch;
/// [`ReviewCliError::Vdiff`] if `vdiff --export-comments` itself failed
/// (not `PATH`, spawn failure, or a nonzero exit — an empty store is
/// [`FixOutcome::NoComments`], not an error); [`ReviewCliError::Prepare`] if
/// [`prepare_review_fix`]'s preflight failed.
pub fn fix(
    deps: &ReviewFixDeps<'_>,
    paths: &RunLanePaths,
    key: &str,
    fg: bool,
    out: &mut dyn Write,
) -> Result<FixOutcome, ReviewCliError> {
    let run = resolve_run(deps.run_store, key, Some("lane"))?;
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
    let pid = if fg { Some(std::process::id()) } else { None };

    let prepared: PreparedRun = prepare_review_fix(
        deps.git,
        deps.run_store,
        deps.clock,
        paths,
        &run.ticket,
        &run.lane,
        &worktree,
        &branch,
        prompt,
        pid,
    )?;

    if fg {
        let outcome = run_claude_and_finish(deps.spawner, deps.gh, deps.run_store, &prepared, out)?;
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
    writeln!(out, "watch:    tm runs watch")?;

    Ok(FixOutcome::Dispatched { succeeded: true })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::gh_cli::FakeGhCli;
    use crate::runs::StartRun;
    use crate::work::detach::FakeDetachSpawner;
    use crate::work::git::FakeGitOps;
    use crate::work::run::FakeClock;
    use crate::work::runner::FakeProcessSpawner;
    use crate::work::vdiff::FakeVdiffOps;
    use tempfile::TempDir;

    fn setup() -> (TempDir, RunStore, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let worktree = tmp.path().join("Worktrees/axiom/proj-1");
        std::fs::create_dir_all(&worktree).unwrap();
        (tmp, run_store, worktree)
    }

    fn seed_lane_run(run_store: &RunStore, worktree: &Path) {
        run_store
            .start_run(&StartRun {
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
        };
        let paths = paths(&tmp);
        let mut out = Vec::new();

        let err = fix(&deps, &paths, "PROJ-1", false, &mut out).unwrap_err();

        assert!(matches!(err, ReviewCliError::Runs(_)));
    }

    #[test]
    fn fix_errors_when_the_lane_run_has_no_branch() {
        let (tmp, run_store, worktree) = setup();
        run_store
            .start_run(&StartRun {
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
        };
        let paths = paths(&tmp);
        let mut out = Vec::new();

        let err = fix(&deps, &paths, "PROJ-1", false, &mut out).unwrap_err();

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
        };
        let paths = paths(&tmp);
        let mut out = Vec::new();

        let outcome = fix(&deps, &paths, "PROJ-1", false, &mut out).unwrap();

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
        };
        let paths = paths(&tmp);
        let mut out = Vec::new();

        let err = fix(&deps, &paths, "PROJ-1", false, &mut out).unwrap_err();

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
        };
        let paths = paths(&tmp);
        let mut out = Vec::new();

        let outcome = fix(&deps, &paths, "PROJ-1", false, &mut out).unwrap();

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
            .latest_run_for_ticket_kind("PROJ-1", Some("review-fix"))
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
        };
        let paths = paths(&tmp);
        let mut out = Vec::new();

        let outcome = fix(&deps, &paths, "PROJ-1", true, &mut out).unwrap();

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
            .latest_run_for_ticket_kind("PROJ-1", Some("review-fix"))
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
        };
        let paths = paths(&tmp);
        let mut out = Vec::new();

        let outcome = fix(&deps, &paths, "PROJ-1", true, &mut out).unwrap();

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
        };
        let paths = paths(&tmp);
        let mut out = Vec::new();

        let err = fix(&deps, &paths, "PROJ-1", false, &mut out).unwrap_err();

        assert!(matches!(err, ReviewCliError::Prepare(_)));
        assert!(detach.recorded.lock().unwrap().is_empty());
        // Only the seeded lane run -- no review-fix row was created.
        assert_eq!(run_store.list_runs().unwrap().len(), 1);
    }
}
