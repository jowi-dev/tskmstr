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

use std::cell::RefCell;
use std::io::Write;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::agent::{AgentRunner, RunMode};
use crate::config::AutoReviewConfig;
use crate::github::gh_cli::GhCli;
use crate::runs::{RunStatus, RunStore, RunStoreError};
use crate::work::auto_review::{
    AUTO_REVIEW_KIND, AutoReviewError, AutoReviewOps, DEFAULT_PROMPT, LoopOutcome, run_loop,
};
use crate::work::detach::{DetachError, DetachSpawner, SupervisorState, supervisor_argv};
use crate::work::git::{GitError, GitOps};
use crate::work::interactive::{
    FIX_WINDOW_NAME, InteractiveLaunchError, launch_interactive_run, resolve_action_window,
};
use crate::work::prompt::{PromptFileError, read_prompt_file};
use crate::work::run::{
    Clock, ExistingBranchRun, PreparedRun, RunLaneError, RunLanePaths, prepare_existing_branch_run,
    prepare_review_fix, run_agent_and_finish, run_log_path,
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

    /// `tm review auto`: an auto-review round for this ticket is already
    /// running; a second loop would race it over the same comment store.
    #[error("an auto-review round for {ticket} is already running (run {run_id})")]
    AutoReviewRunning {
        /// The ticket key.
        ticket: String,
        /// The running round's run id.
        run_id: i64,
    },

    /// `tm review auto`: `[work.auto_review].prompt_file` couldn't be read.
    #[error(transparent)]
    PromptFile(#[from] PromptFileError),

    /// `tm review auto`: resolving the worktree's git dir failed.
    #[error(transparent)]
    Git(#[from] GitError),

    /// `tm review auto`: a round step failed.
    #[error(transparent)]
    AutoReview(#[from] AutoReviewError),

    /// `tm review import`: no pull request is open for the lane's branch.
    #[error("no pull request is open for {branch} — nothing to import a review from")]
    NoPullRequest {
        /// The lane run's branch.
        branch: String,
    },

    /// `tm review import`: a `gh` call failed.
    #[error(transparent)]
    Gh(#[from] crate::github::gh_cli::GhError),

    /// `tm review import`: reading or writing the comment store or the
    /// import sidecar failed.
    #[error(transparent)]
    CommentStore(#[from] crate::work::comment_store::CommentStoreError),
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
    /// The loaded config, or `None` when it could not load. A fix or
    /// auto-review run resolves its model from the lane's `model` through
    /// the shared chain ([`review_run_model`], GitHub issue #143).
    pub config: Option<&'a crate::config::Config>,
}

/// The model a fix pass or auto-review round on `lane` passes: the lane's
/// own `model`, then `[work] default_model`, `[agent.<runner>]
/// default_model` and the built-in default, per
/// [`crate::work::model::resolve_model`]. With no config loaded, the
/// built-in default.
fn review_run_model(deps: &ReviewFixDeps<'_>, lane: &str) -> String {
    let lane_model = deps
        .config
        .and_then(|cfg| cfg.work.lanes.get(lane))
        .and_then(|l| l.model.as_deref());
    crate::work::model::ModelDefaults::from_optional(deps.config)
        .resolve(
            deps.runner,
            &[crate::work::model::ModelCandidate {
                source: "lane model",
                model: lane_model,
            }],
        )
        .model
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
        review_run_model(deps, &run.lane),
        pid,
        dispatch.run_mode(),
        deps.runner,
    )?;

    if dispatch == Dispatch::Interactive {
        let target = target
            .as_ref()
            .expect("an interactive dispatch always resolves a window");
        // Same post-`start_run` stamp as `tm work run`'s interactive path:
        // a killed ticket session reaps this row immediately (issue #26).
        deps.run_store
            .update_tmux_session(prepared.run_id, &target.session_name)?;
        let prompt_path = paths.state_dir.join(format!(
            "{}-{}.prompt.md",
            prepared.wt_name, prepared.timestamp
        ));
        launch_interactive_run(deps.tmux, target, &prepared, &prompt_path, deps.runner)?;
        crate::work::hibernate::record_launch(
            deps.run_store,
            prepared.run_id,
            &crate::work::hibernate::LaunchRecord {
                window: target.window_name.clone(),
                resume: deps.runner.resume_spec(&prepared.invocation),
            },
        )?;

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
            None,
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

/// How a `tm review auto` invocation ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoOutcome {
    /// The loop was handed to a detached `--foreground` child logging to
    /// `log_path`.
    Detached {
        /// The child's log file.
        log_path: PathBuf,
    },
    /// The loop ran here, to completion.
    Finished(LoopOutcome),
}

/// The detached auto-review loop's log file for `key`:
/// `~/.local/state/tskmstr/auto-review/<key>.log`, one per ticket.
pub fn auto_review_log_path(home: &Path, key: &str) -> PathBuf {
    home.join(".local")
        .join("state")
        .join("tskmstr")
        .join("auto-review")
        .join(format!("{}.log", key.to_lowercase()))
}

/// The argv a detached auto-review loop for `key` is re-exec'd with.
pub fn detached_auto_review_argv(key: &str) -> Vec<String> {
    vec![
        "review".to_string(),
        "auto".to_string(),
        key.to_string(),
        "--foreground".to_string(),
    ]
}

/// The PR number at the end of a GitHub pull-request URL.
fn pr_number_from_url(url: &str) -> Option<u64> {
    url.trim_end_matches('/').rsplit('/').next()?.parse().ok()
}

/// `tm review auto <KEY> [--foreground]` (GitHub issue #124): see
/// `docs/plans/gh-124-auto-review.md`.
///
/// Without `foreground`, refuses if an auto-review round for KEY is already
/// running, then re-execs a detached `tm review auto <KEY> --foreground` in
/// the ticket's worktree, the same shape as `tm pr watch`. With it, runs
/// [`run_loop`] here: each review round is a headless `auto-review` run on
/// the ticket's lane worktree and branch, and each fix round is [`fix`]
/// with [`Dispatch::HeadlessForeground`]. A round that needs a fix after the
/// ticket already has `max_fix_rounds` `review-fix` runs (`[dispatch]`,
/// see [`crate::work::fix_cap`]) ends the loop as capped instead. Under
/// autopilot, each fix round first imports the PR's unresolved
/// `review_bots` threads ([`import_bot_threads`], GitHub issue #145).
///
/// # Errors
///
/// [`ReviewCliError::Runs`]/[`ReviewCliError::MissingBranch`] as for
/// [`fix`]; [`ReviewCliError::AutoReviewRunning`] for a second loop;
/// [`ReviewCliError::PromptFile`] for an unreadable `prompt_file`;
/// [`ReviewCliError::AutoReview`] if any round step fails.
#[allow(clippy::too_many_arguments)]
pub fn auto(
    deps: &ReviewFixDeps<'_>,
    paths: &RunLanePaths,
    cfg: &AutoReviewConfig,
    max_fix_rounds: u32,
    review_bots: &[String],
    key: &str,
    foreground: bool,
    out: &mut dyn Write,
) -> Result<AutoOutcome, ReviewCliError> {
    let scope = deps.backend_identity.scope();
    let run = resolve_run(deps.run_store, Some(&scope), key, Some("lane"))?;
    let branch = run
        .branch
        .clone()
        .ok_or_else(|| ReviewCliError::MissingBranch {
            ticket: run.ticket.clone(),
        })?;
    let worktree = PathBuf::from(&run.worktree);

    if !foreground {
        let live = deps.run_store.latest_run_for_ticket_kind(
            Some(&scope),
            &run.ticket,
            Some(AUTO_REVIEW_KIND),
        )?;
        if let Some(live) = live.filter(|r| r.status == RunStatus::Running) {
            return Err(ReviewCliError::AutoReviewRunning {
                ticket: run.ticket.clone(),
                run_id: live.id,
            });
        }
        let log_path = auto_review_log_path(&paths.home, &run.ticket);
        if let Some(dir) = log_path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        deps.detach.spawn_detached(
            deps.current_exe,
            &detached_auto_review_argv(&run.ticket),
            &worktree,
            &log_path,
        )?;
        writeln!(out, "started   auto-review {} on {branch}", run.ticket)?;
        writeln!(out, "log       {}", log_path.display())?;
        return Ok(AutoOutcome::Detached { log_path });
    }

    let template = match (&cfg.prompt, &cfg.prompt_file) {
        (Some(prompt), _) => prompt.clone(),
        (None, Some(file)) => {
            read_prompt_file(file, &paths.home, "[work.auto_review].prompt_file")?
        }
        (None, None) => DEFAULT_PROMPT.to_string(),
    };
    let git_dir = deps.git.git_dir(&worktree)?;
    let ops = CliAutoReviewOps {
        deps,
        paths,
        scope: &scope,
        ticket: &run.ticket,
        lane: &run.lane,
        worktree: &worktree,
        branch: &branch,
        git_dir: &git_dir,
        review_bots,
        max_fix_rounds,
        out: RefCell::new(out),
    };
    let outcome = run_loop(&ops, cfg, &template, &run.ticket, &git_dir)?;
    let out = ops.out.into_inner();
    writeln!(
        out,
        "auto-review {} finished after {} round(s): {}",
        run.ticket,
        outcome.rounds,
        outcome.verdict.as_str()
    )?;
    Ok(AutoOutcome::Finished(outcome))
}

/// Imports the unresolved `review_bots` threads on `branch`'s PR into the
/// comment store under `git_dir`, for an auto-review fix round to address
/// alongside its own findings (GitHub issue #145). Only while `scope`'s
/// autopilot flag is on; otherwise the bots' threads stay `tm pr watch`'s.
/// Returns how many threads were imported.
///
/// A review bot must never stop a review, so a missing PR or a failed `gh`
/// call imports nothing (the latter with a warning on `out`) rather than
/// failing the round.
///
/// # Errors
///
/// [`ReviewCliError::Runs`] if the autopilot flag can't be read;
/// [`ReviewCliError::CommentStore`] for a store or sidecar failure.
pub fn import_bot_threads(
    deps: &ReviewFixDeps<'_>,
    scope: &str,
    branch: &str,
    worktree: &Path,
    git_dir: &Path,
    review_bots: &[String],
    out: &mut dyn Write,
) -> Result<usize, ReviewCliError> {
    use crate::work::{comment_store, review_import};

    if review_bots.is_empty() || !deps.run_store.autopilot_enabled(scope)? {
        return Ok(0);
    }
    let threads = deps.gh.pr_url_for_branch(branch).and_then(|url| {
        match url.as_deref().and_then(pr_number_from_url) {
            Some(pr) => deps.gh.pr_review_threads_full(worktree, pr).map(Some),
            None => Ok(None),
        }
    });
    let threads = match threads {
        Ok(Some(threads)) => threads,
        Ok(None) => return Ok(0),
        Err(err) => {
            writeln!(
                out,
                "warning: could not read review-bot threads for {branch}: {err}"
            )?;
            return Ok(0);
        }
    };
    let created_at = comment_store::iso8601_utc(deps.clock.now_unix_secs());
    let report = review_import::import_bots(git_dir, &threads.threads, review_bots, &created_at)?;
    let imported = report.imported.len();
    if imported > 0 {
        writeln!(out, "imported  {imported} review-bot thread(s) for the fix")?;
    }
    Ok(imported)
}

/// [`AutoReviewOps`] over the real run store, agent runner and `vdiff`.
struct CliAutoReviewOps<'a, 'o> {
    deps: &'a ReviewFixDeps<'a>,
    paths: &'a RunLanePaths,
    scope: &'a str,
    ticket: &'a str,
    lane: &'a str,
    worktree: &'a Path,
    branch: &'a str,
    git_dir: &'a Path,
    review_bots: &'a [String],
    max_fix_rounds: u32,
    out: RefCell<&'o mut dyn Write>,
}

impl AutoReviewOps for CliAutoReviewOps<'_, '_> {
    fn review(&self, round: u32, prompt: String) -> Result<i64, AutoReviewError> {
        let deps = self.deps;
        let prepared = prepare_existing_branch_run(
            deps.git,
            deps.run_store,
            deps.clock,
            self.paths,
            ExistingBranchRun {
                kind: AUTO_REVIEW_KIND,
                scope: self.scope,
                ticket: self.ticket,
                lane: self.lane,
                worktree: self.worktree,
                branch: self.branch,
                prompt,
                model: review_run_model(deps, self.lane),
                pid: Some(std::process::id()),
                mode: RunMode::Headless,
            },
            deps.runner,
        )
        .map_err(|e| AutoReviewError::step("review", e))?;
        let mut out = self.out.borrow_mut();
        writeln!(out, "auto-review round {round}: run {}", prepared.run_id)
            .map_err(|e| AutoReviewError::step("review", e))?;
        let outcome = run_agent_and_finish(
            deps.spawner,
            deps.gh,
            deps.run_store,
            None,
            &prepared,
            deps.runner,
            &mut **out,
        )
        .map_err(|e| AutoReviewError::step("review", e))?;
        if outcome.is_error {
            return Err(AutoReviewError::step(
                "review",
                format!("round {round}'s agent run {} failed", prepared.run_id),
            ));
        }
        Ok(prepared.run_id)
    }

    fn fix(&self) -> Result<bool, AutoReviewError> {
        let mut out = self.out.borrow_mut();
        match fix(
            self.deps,
            self.paths,
            self.ticket,
            Dispatch::HeadlessForeground,
            &mut **out,
        ) {
            Ok(FixOutcome::Dispatched { succeeded }) => Ok(succeeded),
            Ok(FixOutcome::NoComments) => Ok(true),
            Err(err) => Err(AutoReviewError::step("fix", err)),
        }
    }

    fn publish(&self) -> Result<(), AutoReviewError> {
        let url = self
            .deps
            .gh
            .pr_url_for_branch(self.branch)
            .map_err(|e| AutoReviewError::step("publish", e))?
            .ok_or_else(|| {
                AutoReviewError::step("publish", format!("no PR is open for {}", self.branch))
            })?;
        let pr = pr_number_from_url(&url)
            .ok_or_else(|| AutoReviewError::step("publish", format!("no PR number in {url}")))?;
        self.deps
            .vdiff
            .publish_comments(self.worktree, pr)
            .map_err(|e| AutoReviewError::step("publish", e))
    }

    fn record(&self, run_id: i64, kind: &str, detail: &str) -> Result<(), AutoReviewError> {
        self.deps
            .run_store
            .add_event(run_id, kind, Some(detail))
            .map(|_| ())
            .map_err(|e| AutoReviewError::step("record", e))
    }

    fn flag(&self, run_id: i64) -> Result<(), AutoReviewError> {
        self.deps
            .run_store
            .flag_for_review(run_id)
            .map_err(|e| AutoReviewError::step("record", e))
    }

    fn fix_cap_reached(&self) -> Result<bool, AutoReviewError> {
        crate::work::fix_cap::fix_cap_reached(
            self.deps.run_store,
            self.scope,
            self.ticket,
            self.max_fix_rounds,
        )
        .map_err(|e| AutoReviewError::step("fix", e))
    }

    fn now_unix_secs(&self) -> i64 {
        self.deps.clock.now_unix_secs()
    }

    fn import_bot_comments(&self) -> Result<usize, AutoReviewError> {
        let mut out = self.out.borrow_mut();
        import_bot_threads(
            self.deps,
            self.scope,
            self.branch,
            self.worktree,
            self.git_dir,
            self.review_bots,
            &mut **out,
        )
        .map_err(|e| AutoReviewError::step("import", e))
    }
}

/// The settings `tm review import` reads from config.
#[derive(Debug, Clone, Copy)]
pub struct ImportSettings<'a> {
    /// `review_bots`: their threads are `tm pr watch`'s, never imported.
    pub review_bots: &'a [String],
    /// `[dispatch] max_fix_rounds`; see [`crate::work::fix_cap`].
    pub max_fix_rounds: u32,
    /// `[review] resolve_imported`.
    pub resolve_imported: bool,
}

/// What `tm review import --fix` did after importing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportFix {
    /// `--fix` wasn't given.
    NotRequested,
    /// The store holds no imported GitHub comment, so there was nothing to
    /// fix.
    NothingToFix,
    /// The ticket already has `max_fix_rounds` fix passes; none was
    /// started and the lane run was flagged for review.
    Capped,
    /// The fix pass ran and failed. The store is left in place.
    Failed,
    /// The fix pass succeeded and the store was archived. `resolved` is how
    /// many threads were replied to and resolved on GitHub.
    Fixed {
        /// Threads resolved (always `0` unless `resolve_imported`).
        resolved: usize,
    },
}

/// How one `tm review import` went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportOutcome {
    /// Threads imported by this pass.
    pub imported: usize,
    /// What `--fix` did.
    pub fix: ImportFix,
}

/// `tm review import <KEY> [--fix]` (GitHub issue #120): see
/// `docs/plans/gh-120-import-github-review.md`.
///
/// Imports the unresolved human review threads on the PR for KEY's lane
/// branch into the worktree's comment store
/// ([`crate::work::review_import::import`]) and records an
/// [`IMPORT_EVENT`](crate::work::review_import::IMPORT_EVENT) on the lane
/// run when any were imported. With `fix`, and when the store holds any
/// imported comment, it then runs an unchanged [`fix`] in the foreground,
/// unless the ticket has used up `max_fix_rounds`. A successful fix pass
/// archives the store, and with `resolve_imported` replies "Addressed in
/// <sha>" on each imported thread and resolves it, but only once the fix
/// pass has pushed (HEAD moved and matches its upstream).
///
/// # Errors
///
/// [`ReviewCliError::Runs`]/[`ReviewCliError::MissingBranch`] as for
/// [`fix`]; [`ReviewCliError::NoPullRequest`] when no PR is open for the
/// branch; [`ReviewCliError::Gh`] if fetching the threads fails;
/// [`ReviewCliError::CommentStore`] for a store or sidecar failure; any
/// [`fix`] error. A failed reply or resolve is reported, not returned.
pub fn import(
    deps: &ReviewFixDeps<'_>,
    paths: &RunLanePaths,
    settings: &ImportSettings<'_>,
    key: &str,
    fix_requested: bool,
    out: &mut dyn Write,
) -> Result<ImportOutcome, ReviewCliError> {
    use crate::work::{comment_store, fix_cap, review_import};

    let scope = deps.backend_identity.scope();
    let run = resolve_run(deps.run_store, Some(&scope), key, Some("lane"))?;
    let branch = run
        .branch
        .clone()
        .ok_or_else(|| ReviewCliError::MissingBranch {
            ticket: run.ticket.clone(),
        })?;
    let worktree = PathBuf::from(&run.worktree);
    let no_pr = || ReviewCliError::NoPullRequest {
        branch: branch.clone(),
    };
    let url = deps.gh.pr_url_for_branch(&branch)?.ok_or_else(no_pr)?;
    let pr = pr_number_from_url(&url).ok_or_else(no_pr)?;

    let threads = deps.gh.pr_review_threads_full(&worktree, pr)?;
    let git_dir = deps.git.git_dir(&worktree)?;
    let created_at = comment_store::iso8601_utc(deps.clock.now_unix_secs());
    let report = review_import::import(
        &git_dir,
        &threads.threads,
        settings.review_bots,
        &created_at,
    )?;
    let imported = report.imported.len();
    writeln!(
        out,
        "imported  {imported} GitHub review thread(s) from PR #{pr}"
    )?;
    let record = |detail: &str| {
        deps.run_store
            .add_event(run.id, review_import::IMPORT_EVENT, Some(detail))
            .map(|_| ())
    };
    if imported > 0 {
        record(&format!("{imported} imported"))?;
    }

    let done = |fix| Ok(ImportOutcome { imported, fix });
    if !fix_requested {
        return done(ImportFix::NotRequested);
    }
    if !review_import::store_has_imports(&git_dir)? {
        return done(ImportFix::NothingToFix);
    }
    if fix_cap::fix_cap_reached(deps.run_store, &scope, &run.ticket, settings.max_fix_rounds)? {
        if run.status.is_terminal() {
            deps.run_store.flag_for_review(run.id)?;
        }
        record("capped")?;
        writeln!(
            out,
            "capped    {} has {} fix pass(es) already; not starting another",
            run.ticket, settings.max_fix_rounds
        )?;
        return done(ImportFix::Capped);
    }

    let head_before = deps.git.rev_parse(&worktree, "HEAD").ok();
    match fix(deps, paths, &run.ticket, Dispatch::HeadlessForeground, out)? {
        FixOutcome::NoComments => done(ImportFix::NothingToFix),
        FixOutcome::Dispatched { succeeded: false } => {
            record("fix-failed")?;
            done(ImportFix::Failed)
        }
        FixOutcome::Dispatched { succeeded: true } => {
            let round = fix_cap::fix_rounds_used(deps.run_store, &scope, &run.ticket)?;
            comment_store::archive_named(&git_dir, &format!("import-{round}"))?;
            record("fix")?;
            let resolved = if settings.resolve_imported {
                resolve_imported_threads(deps, &worktree, &git_dir, head_before, out)?
            } else {
                0
            };
            done(ImportFix::Fixed { resolved })
        }
    }
}

/// Replies "Addressed in <sha>" on, and resolves, every imported thread the
/// sidecar hasn't marked resolved, when the fix pass pushed: HEAD moved off
/// `head_before` and equals its upstream. Returns how many were resolved.
fn resolve_imported_threads(
    deps: &ReviewFixDeps<'_>,
    worktree: &Path,
    git_dir: &Path,
    head_before: Option<String>,
    out: &mut dyn Write,
) -> Result<usize, ReviewCliError> {
    use crate::work::review_import;

    let head = deps.git.rev_parse(worktree, "HEAD").ok();
    let upstream = deps.git.rev_parse(worktree, "@{upstream}").ok();
    let Some(sha) =
        head.filter(|h| Some(h) != head_before.as_ref() && Some(h) == upstream.as_ref())
    else {
        writeln!(
            out,
            "not resolving imported threads: the fix pass didn't push a new commit"
        )?;
        return Ok(0);
    };
    let mut sidecar = review_import::load_sidecar(git_dir)?;
    let short = &sha[..sha.len().min(7)];
    let mut resolved = 0;
    for thread in sidecar.unresolved_threads() {
        match deps
            .gh
            .reply_and_resolve_thread(worktree, &thread, &format!("Addressed in {short}."))
        {
            Ok(()) => {
                sidecar.mark_resolved(&thread);
                resolved += 1;
            }
            Err(err) => writeln!(out, "warning: could not resolve thread {thread}: {err}")?,
        }
    }
    review_import::save_sidecar(git_dir, &sidecar)?;
    writeln!(out, "resolved  {resolved} imported thread(s) at {short}")?;
    Ok(resolved)
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
            config: None,
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
            config: None,
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
            config: None,
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
            config: None,
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
            config: None,
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
            config: None,
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
            config: None,
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
        // The hosting session is stamped on the row so the reaper can treat
        // a killed session as proof of death (GitHub issue #26).
        assert_eq!(
            review_fix_run.tmux_session,
            Some("tm-proj-proj-1".to_string())
        );
        // The deduplicated window is what a wake must recreate (issue #64).
        let record = crate::work::hibernate::launch_record(&run_store, review_fix_run.id)
            .unwrap()
            .expect("an interactive fix pass records a resume recipe");
        assert_eq!(record.window, "fix-2");

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
            config: None,
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
            config: None,
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

    /// Runs a foreground `fix` under `config` and returns the agent's
    /// `--model` value.
    fn foreground_fix_model(config: Option<&crate::config::Config>) -> String {
        let (tmp, run_store, worktree) = setup();
        seed_lane_run(&run_store, &worktree);
        let git = FakeGitOps::new();
        let gh = FakeGhCli::new();
        let spawner = FakeProcessSpawner::success(canned_json());
        let clock = FakeClock((2026, 8, 18, 10, 0, 0));
        let vdiff = FakeVdiffOps::with_export(Ok("## a.rs\n\na.rs:1-2 nit".to_string()));
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
            config,
        };
        fix(
            &deps,
            &paths(&tmp),
            "PROJ-1",
            Dispatch::HeadlessForeground,
            &mut Vec::new(),
        )
        .unwrap();
        let recorded = spawner.recorded.lock().unwrap();
        let args = &recorded[0].args;
        let at = args.iter().position(|a| a == "--model").expect("--model");
        args[at + 1].clone()
    }

    fn config_with(extra: &str) -> crate::config::Config {
        let raw = format!(
            "jira_base_url = \"https://x.atlassian.net\"\n\
             jira_email = \"x@example.com\"\n\
             default_project_key = \"PROJ\"\n{extra}"
        );
        crate::config::merge(toml::from_str(&raw).unwrap(), None).unwrap()
    }

    /// GitHub issue #143: a fix pass resolves its model through the shared
    /// chain instead of running the adapter's old hard-coded default.
    #[test]
    fn fix_passes_the_lane_model_then_the_work_default_model() {
        assert_eq!(foreground_fix_model(None), "opus");

        let work_default = config_with("[work]\ndefault_model = \"sonnet\"\n");
        assert_eq!(foreground_fix_model(Some(&work_default)), "sonnet");

        let lane_model = config_with(
            "[work]\ndefault_model = \"sonnet\"\n\
             [work.lanes.mylane]\nrepo = \"/r\"\nmodel = \"fable\"\n",
        );
        assert_eq!(foreground_fix_model(Some(&lane_model)), "fable");
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
            config: None,
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
            config: None,
        };
        let paths = paths(&tmp);
        let mut out = Vec::new();

        let err = fix(&deps, &paths, "PROJ-1", Dispatch::Headless, &mut out).unwrap_err();

        assert!(matches!(err, ReviewCliError::Prepare(_)));
        assert!(detach.recorded.lock().unwrap().is_empty());
        // Only the seeded lane run -- no review-fix row was created.
        assert_eq!(run_store.list_runs().unwrap().len(), 1);
    }

    // --- tm review auto (GitHub issue #124) ---

    /// A fake agent: writes `findings` to `findings_file` (round 1's), then
    /// defers to a [`FakeProcessSpawner`] for the result JSON.
    struct FindingsWritingSpawner {
        inner: FakeProcessSpawner,
        findings_file: PathBuf,
        findings: &'static str,
    }

    impl ProcessSpawner for FindingsWritingSpawner {
        fn spawn(
            &self,
            request: crate::work::runner::SpawnRequest<'_>,
        ) -> Result<std::process::ExitStatus, crate::work::runner::SpawnError> {
            std::fs::write(&self.findings_file, self.findings).unwrap();
            self.inner.spawn(request)
        }
    }

    struct AutoFakes {
        git: FakeGitOps,
        gh: FakeGhCli,
        clock: FakeClock,
        vdiff: FakeVdiffOps,
        detach: FakeDetachSpawner,
        tmux: FakeTmuxOps,
        current_exe: PathBuf,
        run_db_path: PathBuf,
    }

    fn auto_fakes(tmp: &TempDir) -> AutoFakes {
        AutoFakes {
            git: FakeGitOps::new(),
            gh: FakeGhCli::new(),
            clock: FakeClock((2026, 8, 18, 10, 0, 0)),
            vdiff: FakeVdiffOps::with_export(Ok("## src/a.rs\n\ncomment".to_string())),
            detach: FakeDetachSpawner::new(4242),
            tmux: FakeTmuxOps::new(),
            current_exe: PathBuf::from("/usr/local/bin/tm"),
            run_db_path: tmp.path().join("runs.db"),
        }
    }

    fn auto_deps<'a>(
        fakes: &'a AutoFakes,
        run_store: &'a RunStore,
        spawner: &'a dyn ProcessSpawner,
    ) -> ReviewFixDeps<'a> {
        ReviewFixDeps {
            git: &fakes.git,
            gh: &fakes.gh,
            spawner,
            run_store,
            clock: &fakes.clock,
            vdiff: &fakes.vdiff,
            detach: &fakes.detach,
            current_exe: &fakes.current_exe,
            run_db_path: &fakes.run_db_path,
            tmux: &fakes.tmux,
            backend_identity: test_identity(),
            runner: &ClaudeRunner,
            config: None,
        }
    }

    #[test]
    fn auto_detaches_a_foreground_child_in_the_worktree() {
        let (tmp, run_store, worktree) = setup();
        seed_lane_run(&run_store, &worktree);
        let fakes = auto_fakes(&tmp);
        let spawner = FakeProcessSpawner::success(canned_json());
        let deps = auto_deps(&fakes, &run_store, &spawner);
        let paths = paths(&tmp);
        let mut out = Vec::new();

        let outcome = auto(
            &deps,
            &paths,
            &AutoReviewConfig::default(),
            crate::config::DEFAULT_MAX_FIX_ROUNDS,
            &[],
            "proj-1",
            false,
            &mut out,
        )
        .unwrap();

        let log_path = auto_review_log_path(&paths.home, "PROJ-1");
        assert_eq!(
            outcome,
            AutoOutcome::Detached {
                log_path: log_path.clone()
            }
        );
        let recorded = fakes.detach.recorded.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            recorded[0].argv,
            vec!["review", "auto", "PROJ-1", "--foreground"]
        );
        assert_eq!(recorded[0].working_dir, worktree);
        assert_eq!(recorded[0].log_path, log_path);
        assert!(spawner.recorded.lock().unwrap().is_empty());
    }

    #[test]
    fn auto_refuses_a_second_loop_while_a_round_is_running() {
        let (tmp, run_store, worktree) = setup();
        seed_lane_run(&run_store, &worktree);
        run_store
            .start_run(&StartRun {
                scope: String::new(),
                ticket: "PROJ-1".to_string(),
                lane: "mylane".to_string(),
                worktree: worktree.to_string_lossy().into_owned(),
                branch: Some("jowi-dev/proj-1-slug".to_string()),
                pid: None,
                kind: AUTO_REVIEW_KIND.to_string(),
                log_path: None,
            })
            .unwrap();
        let fakes = auto_fakes(&tmp);
        let spawner = FakeProcessSpawner::success(canned_json());
        let deps = auto_deps(&fakes, &run_store, &spawner);
        let mut out = Vec::new();

        let err = auto(
            &deps,
            &paths(&tmp),
            &AutoReviewConfig::default(),
            crate::config::DEFAULT_MAX_FIX_ROUNDS,
            &[],
            "PROJ-1",
            false,
            &mut out,
        )
        .unwrap_err();

        assert!(matches!(err, ReviewCliError::AutoReviewRunning { .. }));
        assert!(fakes.detach.recorded.lock().unwrap().is_empty());
    }

    #[test]
    fn auto_foreground_runs_a_clean_round_and_records_it() {
        let (tmp, run_store, worktree) = setup();
        seed_lane_run(&run_store, &worktree);
        let fakes = auto_fakes(&tmp);
        let git_dir = worktree.join(".git");
        let findings_file = crate::work::auto_review::findings_path(&git_dir, 1);
        let spawner = FindingsWritingSpawner {
            inner: FakeProcessSpawner::success(canned_json()),
            findings_file,
            findings: r#"[{"path": "src/a.rs", "start_line": 2,
                "severity": "LOW", "specialist": "pedantic", "body": "typo"}]"#,
        };
        let deps = auto_deps(&fakes, &run_store, &spawner);
        let mut out = Vec::new();

        let outcome = auto(
            &deps,
            &paths(&tmp),
            &AutoReviewConfig::default(),
            crate::config::DEFAULT_MAX_FIX_ROUNDS,
            &[],
            "PROJ-1",
            true,
            &mut out,
        )
        .unwrap();

        assert_eq!(
            outcome,
            AutoOutcome::Finished(LoopOutcome {
                rounds: 1,
                verdict: crate::work::auto_review::RoundVerdict::Clean,
            })
        );
        let round = run_store
            .latest_run_for_ticket_kind(None, "PROJ-1", Some(AUTO_REVIEW_KIND))
            .unwrap()
            .unwrap();
        assert_eq!(round.worktree, worktree.to_string_lossy());
        assert_eq!(round.branch, Some("jowi-dev/proj-1-slug".to_string()));
        let events = run_store.events_for_run(round.id).unwrap();
        assert!(
            events
                .iter()
                .any(|e| e.kind == "auto_review_round" && e.detail.as_deref() == Some("1/2 clean"))
        );
        let args = &spawner.inner.recorded.lock().unwrap()[0].args;
        assert!(args.iter().any(|a| a.contains("thatch-code-review")));
        let store = crate::work::comment_store::load(&git_dir).unwrap();
        assert_eq!(store[0]["text"], "[auto-review · pedantic · LOW] typo");
        assert!(
            run_store
                .latest_run_for_ticket_kind(None, "PROJ-1", Some("review-fix"))
                .unwrap()
                .is_none(),
            "a LOW finding never triggers a fix"
        );
    }

    #[test]
    fn auto_foreground_errors_when_the_agent_writes_no_findings() {
        let (tmp, run_store, worktree) = setup();
        seed_lane_run(&run_store, &worktree);
        let fakes = auto_fakes(&tmp);
        let spawner = FakeProcessSpawner::success(canned_json());
        let deps = auto_deps(&fakes, &run_store, &spawner);
        let mut out = Vec::new();

        let err = auto(
            &deps,
            &paths(&tmp),
            &AutoReviewConfig::default(),
            crate::config::DEFAULT_MAX_FIX_ROUNDS,
            &[],
            "PROJ-1",
            true,
            &mut out,
        )
        .unwrap_err();

        assert!(matches!(
            err,
            ReviewCliError::AutoReview(AutoReviewError::Findings { .. })
        ));
    }

    #[test]
    fn pr_number_from_url_takes_the_trailing_number() {
        assert_eq!(
            pr_number_from_url("https://github.com/o/r/pull/131"),
            Some(131)
        );
        assert_eq!(pr_number_from_url("https://github.com/o/r/pull/"), None);
    }

    // --- bugbot threads feeding auto-review fixes (GitHub issue #145) ---

    fn bot_thread_fakes(tmp: &TempDir, pr_url: Option<&str>) -> AutoFakes {
        let comment = crate::github::review_threads::ThreadComment {
            id: 77,
            author_login: Some("cursor".to_string()),
            body: "off by one".to_string(),
            url: "https://github.com/o/r/pull/140#discussion_r77".to_string(),
            created_at: "2026-10-20T10:00:00Z".to_string(),
            original_commit: Some("abcdef0123".to_string()),
        };
        let thread = crate::github::review_threads::FullReviewThread {
            id: "PRRT_bot".to_string(),
            is_resolved: false,
            is_outdated: false,
            path: "src/a.rs".to_string(),
            line: Some(3),
            start_line: None,
            original_line: Some(3),
            original_start_line: None,
            comments: vec![comment],
        };
        AutoFakes {
            gh: FakeGhCli::new()
                .with_pr_url_for_branch(Ok(pr_url.map(str::to_string)))
                .with_pr_review_threads_full(
                    140,
                    Ok(crate::github::review_threads::PrReviewThreads {
                        pr_author: Some("jowi-dev".to_string()),
                        threads: vec![thread],
                    }),
                ),
            ..auto_fakes(tmp)
        }
    }

    const BOT_PR: &str = "https://github.com/o/r/pull/140";

    fn import_bot_threads_with(
        fakes: &AutoFakes,
        run_store: &RunStore,
        git_dir: &Path,
    ) -> (usize, String) {
        let spawner = FakeProcessSpawner::success(canned_json());
        let deps = auto_deps(fakes, run_store, &spawner);
        let mut out = Vec::new();
        let n = import_bot_threads(
            &deps,
            "",
            "jowi-dev/proj-1-slug",
            Path::new("/wt"),
            git_dir,
            &["cursor[bot]".to_string()],
            &mut out,
        )
        .unwrap();
        (n, String::from_utf8(out).unwrap())
    }

    #[test]
    fn import_bot_threads_imports_bot_comments_once_under_autopilot() {
        let (tmp, run_store, _) = setup();
        run_store.set_autopilot("", true).unwrap();
        let fakes = bot_thread_fakes(&tmp, Some(BOT_PR));
        let git_dir = tmp.path().join("git");

        let (first, printed) = import_bot_threads_with(&fakes, &run_store, &git_dir);
        let (again, _) = import_bot_threads_with(&fakes, &run_store, &git_dir);

        assert_eq!((first, again), (1, 0));
        assert!(printed.contains("1 review-bot thread"), "{printed}");
        let store = crate::work::comment_store::load(&git_dir).unwrap();
        assert_eq!(store.len(), 1);
        assert!(
            store[0]["text"]
                .as_str()
                .unwrap()
                .starts_with("[github · @cursor] off by one")
        );
    }

    #[test]
    fn import_bot_threads_does_nothing_when_autopilot_is_off() {
        let (tmp, run_store, _) = setup();
        let fakes = bot_thread_fakes(&tmp, Some(BOT_PR));
        let git_dir = tmp.path().join("git");

        let (n, _) = import_bot_threads_with(&fakes, &run_store, &git_dir);

        assert_eq!(n, 0);
        assert!(fakes.gh.pr_review_threads_full_calls().is_empty());
        assert!(
            crate::work::comment_store::load(&git_dir)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn import_bot_threads_skips_quietly_without_a_pr() {
        let (tmp, run_store, _) = setup();
        run_store.set_autopilot("", true).unwrap();
        let fakes = bot_thread_fakes(&tmp, None);

        let (n, _) = import_bot_threads_with(&fakes, &run_store, &tmp.path().join("git"));

        assert_eq!(n, 0);
    }

    #[test]
    fn import_bot_threads_warns_and_carries_on_when_gh_fails() {
        let (tmp, run_store, _) = setup();
        run_store.set_autopilot("", true).unwrap();
        let fakes = AutoFakes {
            gh: FakeGhCli::new().with_pr_url_for_branch(Err(
                crate::github::gh_cli::GhError::Command {
                    command: "gh pr view".to_string(),
                    exit_code: Some(1),
                    stderr: "rate limited".to_string(),
                },
            )),
            ..auto_fakes(&tmp)
        };

        let (n, printed) = import_bot_threads_with(&fakes, &run_store, &tmp.path().join("git"));

        assert_eq!(n, 0);
        assert!(printed.contains("warning"), "{printed}");
    }

    // --- tm review import (GitHub issue #120) ---

    const RECORDED_THREADS: &str = include_str!("../github/fixtures/review_threads_import.json");

    fn import_fakes(tmp: &TempDir, git: FakeGitOps) -> AutoFakes {
        let threads =
            crate::github::review_threads::parse_review_threads_full(RECORDED_THREADS, 140)
                .unwrap();
        AutoFakes {
            git,
            gh: FakeGhCli::new()
                .with_pr_url_for_branch(Ok(Some(
                    "https://github.com/jowi-dev/tskmstr/pull/140".to_string(),
                )))
                .with_pr_review_threads_full(140, Ok(threads)),
            ..auto_fakes(tmp)
        }
    }

    fn import_settings(max_fix_rounds: u32, resolve_imported: bool) -> ImportSettings<'static> {
        static BOTS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
        ImportSettings {
            review_bots: BOTS.get_or_init(|| vec!["cursor[bot]".to_string()]),
            max_fix_rounds,
            resolve_imported,
        }
    }

    fn lane_run_id(run_store: &RunStore) -> i64 {
        run_store
            .latest_run_for_ticket_kind(None, "PROJ-1", Some("lane"))
            .unwrap()
            .unwrap()
            .id
    }

    fn import_events(run_store: &RunStore) -> Vec<String> {
        run_store
            .events_for_run(lane_run_id(run_store))
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == crate::work::review_import::IMPORT_EVENT)
            .filter_map(|e| e.detail)
            .collect()
    }

    fn finish_lane_run(run_store: &RunStore) {
        run_store
            .finish_run(
                lane_run_id(run_store),
                &crate::runs::FinishRun {
                    status: RunStatus::Done,
                    exit_code: Some(0),
                    session_id: None,
                    cost_usd: None,
                    num_turns: None,
                    blocker: None,
                    pr_url: None,
                    transcript: None,
                    model_usage: None,
                    findings_count: None,
                },
            )
            .unwrap();
    }

    #[test]
    fn import_without_fix_imports_once_badges_and_starts_no_fix() {
        let (tmp, run_store, worktree) = setup();
        seed_lane_run(&run_store, &worktree);
        let fakes = import_fakes(&tmp, FakeGitOps::new());
        let spawner = FakeProcessSpawner::success(canned_json());
        let deps = auto_deps(&fakes, &run_store, &spawner);
        let settings = import_settings(3, false);
        let mut out = Vec::new();

        let first = import(&deps, &paths(&tmp), &settings, "PROJ-1", false, &mut out).unwrap();
        let again = import(&deps, &paths(&tmp), &settings, "PROJ-1", false, &mut out).unwrap();

        assert_eq!(
            first,
            ImportOutcome {
                imported: 3,
                fix: ImportFix::NotRequested
            }
        );
        assert_eq!(again.imported, 0);
        assert_eq!(import_events(&run_store), ["3 imported"]);
        let git_dir = worktree.join(".git");
        assert_eq!(crate::work::comment_store::load(&git_dir).unwrap().len(), 3);
        assert!(
            run_store
                .latest_run_for_ticket_kind(None, "PROJ-1", Some("review-fix"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn import_errors_when_no_pr_is_open_for_the_branch() {
        let (tmp, run_store, worktree) = setup();
        seed_lane_run(&run_store, &worktree);
        let fakes = AutoFakes {
            gh: FakeGhCli::new().with_pr_url_for_branch(Ok(None)),
            ..auto_fakes(&tmp)
        };
        let spawner = FakeProcessSpawner::success(canned_json());
        let deps = auto_deps(&fakes, &run_store, &spawner);
        let mut out = Vec::new();

        let err = import(
            &deps,
            &paths(&tmp),
            &import_settings(3, false),
            "PROJ-1",
            false,
            &mut out,
        )
        .unwrap_err();

        assert!(matches!(err, ReviewCliError::NoPullRequest { .. }), "{err}");
    }

    #[test]
    fn import_fix_runs_an_unchanged_fix_pass_and_archives_the_store() {
        let (tmp, run_store, worktree) = setup();
        seed_lane_run(&run_store, &worktree);
        let fakes = import_fakes(&tmp, FakeGitOps::new());
        let spawner = FakeProcessSpawner::success(canned_json());
        let deps = auto_deps(&fakes, &run_store, &spawner);
        let mut out = Vec::new();

        let outcome = import(
            &deps,
            &paths(&tmp),
            &import_settings(3, false),
            "PROJ-1",
            true,
            &mut out,
        )
        .unwrap();

        assert_eq!(outcome.fix, ImportFix::Fixed { resolved: 0 });
        let fix_run = run_store
            .latest_run_for_ticket_kind(None, "PROJ-1", Some("review-fix"))
            .unwrap()
            .unwrap();
        assert_eq!(fix_run.status, RunStatus::Done);
        let git_dir = worktree.join(".git");
        assert!(
            crate::work::comment_store::load(&git_dir)
                .unwrap()
                .is_empty()
        );
        assert!(crate::work::comment_store::named_archive_path(&git_dir, "import-1").exists());
        assert_eq!(import_events(&run_store), ["3 imported", "fix"]);
        assert!(fakes.gh.reply_and_resolve_thread_calls().is_empty());
    }

    #[test]
    fn import_fix_at_the_cap_flags_the_lane_and_starts_nothing() {
        let (tmp, run_store, worktree) = setup();
        seed_lane_run(&run_store, &worktree);
        finish_lane_run(&run_store);
        run_store
            .start_run(&StartRun {
                scope: String::new(),
                ticket: "PROJ-1".to_string(),
                lane: "mylane".to_string(),
                worktree: worktree.to_string_lossy().into_owned(),
                branch: Some("jowi-dev/proj-1-slug".to_string()),
                pid: None,
                kind: "review-fix".to_string(),
                log_path: None,
            })
            .unwrap();
        let fakes = import_fakes(&tmp, FakeGitOps::new());
        let spawner = FakeProcessSpawner::success(canned_json());
        let deps = auto_deps(&fakes, &run_store, &spawner);
        let mut out = Vec::new();

        let outcome = import(
            &deps,
            &paths(&tmp),
            &import_settings(1, false),
            "PROJ-1",
            true,
            &mut out,
        )
        .unwrap();

        assert_eq!(outcome.fix, ImportFix::Capped);
        assert!(
            spawner.recorded.lock().unwrap().is_empty(),
            "no fix agent ran"
        );
        let lane = run_store
            .run_by_id(lane_run_id(&run_store))
            .unwrap()
            .unwrap();
        assert_eq!(lane.status, RunStatus::Review);
        assert_eq!(import_events(&run_store), ["3 imported", "capped"]);
        let git_dir = worktree.join(".git");
        assert_eq!(
            crate::work::comment_store::load(&git_dir).unwrap().len(),
            3,
            "the store is left for the human"
        );
    }

    #[test]
    fn import_fix_resolves_threads_once_the_fix_pass_pushed() {
        let (tmp, run_store, worktree) = setup();
        seed_lane_run(&run_store, &worktree);
        let git = FakeGitOps::new()
            .with_rev_parse_sequence(
                "HEAD",
                vec![Ok("aaaaaaa1111".to_string()), Ok("bbbbbbb2222".to_string())],
            )
            .with_rev_parse_result("@{upstream}", Ok("bbbbbbb2222".to_string()));
        let fakes = import_fakes(&tmp, git);
        let spawner = FakeProcessSpawner::success(canned_json());
        let deps = auto_deps(&fakes, &run_store, &spawner);
        let mut out = Vec::new();

        let outcome = import(
            &deps,
            &paths(&tmp),
            &import_settings(3, true),
            "PROJ-1",
            true,
            &mut out,
        )
        .unwrap();

        assert_eq!(outcome.fix, ImportFix::Fixed { resolved: 3 });
        let calls = fakes.gh.reply_and_resolve_thread_calls();
        assert_eq!(calls.len(), 3);
        assert!(
            calls
                .iter()
                .all(|(_, body)| body == "Addressed in bbbbbbb.")
        );
        let sidecar = crate::work::review_import::load_sidecar(&worktree.join(".git")).unwrap();
        assert!(sidecar.unresolved_threads().is_empty());
    }

    #[test]
    fn import_fix_leaves_threads_open_when_the_fix_pass_did_not_push() {
        let (tmp, run_store, worktree) = setup();
        seed_lane_run(&run_store, &worktree);
        let git = FakeGitOps::new()
            .with_rev_parse_sequence(
                "HEAD",
                vec![Ok("aaaaaaa1111".to_string()), Ok("bbbbbbb2222".to_string())],
            )
            .with_rev_parse_result("@{upstream}", Ok("aaaaaaa1111".to_string()));
        let fakes = import_fakes(&tmp, git);
        let spawner = FakeProcessSpawner::success(canned_json());
        let deps = auto_deps(&fakes, &run_store, &spawner);
        let mut out = Vec::new();

        let outcome = import(
            &deps,
            &paths(&tmp),
            &import_settings(3, true),
            "PROJ-1",
            true,
            &mut out,
        )
        .unwrap();

        assert_eq!(outcome.fix, ImportFix::Fixed { resolved: 0 });
        assert!(fakes.gh.reply_and_resolve_thread_calls().is_empty());
    }
}
