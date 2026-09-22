//! `tm merge`: rebase a ticket's PR branch onto its base, hand conflicts to
//! an interactive agent session when the rebase can't finish cleanly, then
//! merge the PR and clean up.
//!
//! [`run_merge`] is the whole flow, staged roughly as: resolve the ticket's
//! open PR, locate a local checkout of its branch (if any), fetch and
//! rebase (opening a conflict-resolution tmux session and polling it to
//! completion when the rebase stops on conflicts), merge the PR via `gh`,
//! fast-forward the local base branch, best-effort clean up the worktree
//! and branch, and finally apply the configured `status_on_merge`
//! transition.
//!
//! The conflict-resolution session is launched the same way an interactive
//! lane run is: [`crate::agent::AgentRunner::build_invocation`] under
//! [`crate::agent::RunMode::Interactive`], its prompt written to a file
//! under [`MergeDeps::state_dir`], and
//! [`crate::agent::AgentRunner::tmux_command_line`] rendering the command
//! that reads it back — the same billing-safety `env -u` prefix and
//! unattended default permissions (`bypassPermissions` for `ClaudeRunner`)
//! every other agent session gets. The session itself stays deliberately
//! untracked (no run row): see [`run_conflict_session`].
//!
//! A conflict session that never resolves is not a hard failure: the
//! rebase is deliberately left in progress on disk and
//! [`MergeFlowOutcome::ConflictsHandedBack`] is returned so the caller can
//! say so without treating it as an error.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use thiserror::Error;

use crate::agent::{AgentRunner, InvocationInputs, RunMode};
use crate::cli::pr::{PrCliError, resolve_watch_repo_root};
use crate::config::{BackendIdentity, LaneConfig, MergeConfig};
use crate::github::gh_cli::{GhCli, GhError};
use crate::github::pr::find_pr_for_ticket;
use crate::runs::{RunStore, RunStoreError};
use crate::ticketing::provider::TicketProvider;
use crate::ticketing::{StatusTransition, apply_status_on_merge};
use crate::work::git::{GitError, GitOps, RebaseOutcome};
use crate::work::naming::ticket_session_name;
use crate::work::prompt::{PromptFileError, read_prompt_file};
use crate::work::review_watch::{Clock, Sleeper};
use crate::work::tmux::{
    TmuxError, TmuxOps, has_live_window, session_window_names, unique_window_name,
};

/// Bound on every `gh` shell-out this flow makes (`gh pr merge`).
pub const MERGE_TIMEOUT: Duration = Duration::from_secs(30);

/// Seconds between conflict-session poll ticks.
pub const CONFLICT_POLL_SECS: u64 = 5;

/// Wall-clock seconds the conflict poll loop waits before giving up and
/// handing the rebase back to the caller.
pub const CONFLICT_TIMEOUT_SECS: i64 = 900;

/// Base name (before [`unique_window_name`] suffixing) of the tmux window a
/// conflict-resolution session runs in.
pub const MERGE_WINDOW_BASE: &str = "merge";

/// Errors returned by [`run_merge`].
#[derive(Debug, Error)]
pub enum MergeError {
    /// A `git` shell-out failed.
    #[error(transparent)]
    Git(#[from] GitError),

    /// A `gh` shell-out failed.
    #[error(transparent)]
    Gh(#[from] GhError),

    /// A `tmux` shell-out failed.
    #[error(transparent)]
    Tmux(#[from] TmuxError),

    /// A run-state store operation failed.
    #[error(transparent)]
    Store(#[from] RunStoreError),

    /// A configured `[work.merge].prompt_file` could not be read.
    #[error(transparent)]
    PromptFile(#[from] PromptFileError),

    /// The conflict session's rendered prompt could not be written to
    /// `state_dir` — raised before any tmux call, so a bad `state_dir`
    /// never leaves a half-opened session behind (same stance as a bad
    /// `[work.merge].prompt_file`).
    #[error("failed to write prompt file {path}: {source}")]
    PromptWrite {
        /// The prompt file that could not be written.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// [`resolve_watch_repo_root`] failed to resolve the ticket's repo root.
    #[error(transparent)]
    RepoRoot(#[from] PrCliError),

    /// Writing a status line failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// `key` has no open pull request.
    #[error("no open pull request found for {key}. Run `tm pr create` first.")]
    NoPrForTicket {
        /// The ticket key that has no resolvable open pull request.
        key: String,
    },

    /// The PR's branch has diverged from its remote counterpart (neither is
    /// an ancestor of the other) — not a fast-forward catch-up this flow can
    /// resolve on its own.
    #[error(
        "{branch} has diverged from its own remote counterpart; reconcile manually before merging (git fetch origin && git status)"
    )]
    BranchDiverged {
        /// The diverged branch.
        branch: String,
    },

    /// The local checkout has uncommitted changes, so a rebase can't start.
    #[error(
        "{} has uncommitted changes; commit or stash before merging",
        dir.display()
    )]
    DirtyWorktree {
        /// The dirty checkout directory.
        dir: PathBuf,
    },

    /// `origin/<branch>` no longer exists — the remote branch may have been
    /// deleted.
    #[error("origin/{branch} no longer exists; it may have been deleted upstream")]
    RemoteBranchGone {
        /// The branch whose remote counterpart is gone.
        branch: String,
    },
}

/// What [`run_merge`] accomplished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeFlowOutcome {
    /// The PR was merged (and, best-effort, local cleanup + status
    /// transition applied).
    Merged,
    /// A rebase conflict session never resolved (timed out, or the window
    /// died); the rebase is left in progress on disk and the caller should
    /// attach to finish it by hand.
    ConflictsHandedBack,
}

/// Dependencies [`run_merge`] needs, gathered the same way
/// [`crate::work::review_watch::PollDeps`]/[`crate::work::audit::launch_audit`]'s
/// parameters are.
pub struct MergeDeps<'a> {
    /// Git operations (real or fake).
    pub git: &'a dyn GitOps,
    /// `gh` CLI operations (real or fake).
    pub gh: &'a dyn GhCli,
    /// `tmux` operations (real or fake).
    pub tmux: &'a dyn TmuxOps,
    /// The configured ticket backend.
    pub jira: &'a dyn TicketProvider,
    /// The configured AI coding agent.
    pub runner: &'a dyn AgentRunner,
    /// The run-state store, for locating an existing local checkout of the
    /// PR's branch (see [`resolve_checkout_dir`]) and for
    /// [`resolve_watch_repo_root`]'s lane-based repo-root resolution.
    /// `None` when no store is configured; the repo root then falls back
    /// directly to `git.repo_root(cwd)`, and no run-based checkout
    /// candidate is considered.
    pub run_store: Option<&'a RunStore>,
    /// "Now" source for the conflict poll loop's timeout.
    pub clock: &'a dyn Clock,
    /// Sleep between conflict poll ticks (real or fake).
    pub sleeper: &'a dyn Sleeper,
    /// The invoking process's working directory.
    pub cwd: &'a Path,
    /// The invoking user's home directory, for `~`-expanding
    /// `[work.merge].prompt_file`.
    pub home: &'a Path,
    /// The invoking repo's backend identity, for run-store scoping and the
    /// conflict session's ticket-session name.
    pub identity: &'a BackendIdentity,
    /// Configured lanes, for [`resolve_watch_repo_root`]'s lane-based repo
    /// root resolution.
    pub lanes: &'a BTreeMap<String, LaneConfig>,
    /// Validated `[work.merge]` settings.
    pub merge_cfg: &'a MergeConfig,
    /// Configured `status_on_merge` target status, if any.
    pub status_on_merge: Option<&'a str>,
    /// Directory the conflict session's prompt file is written under
    /// (`merge-<key>-<timestamp>.prompt.md`), the same run-state directory
    /// `tm work run`/`tm review fix` use for their own interactive prompt
    /// files. Created (via `create_dir_all`) if it doesn't exist yet.
    pub state_dir: &'a Path,
}

/// Substitutes `{key}`, `{branch}`, and `{base}` in `template`, producing
/// the prompt text handed to the conflict-resolution agent session.
pub fn conflict_prompt(template: &str, key: &str, branch: &str, base: &str) -> String {
    template
        .replace("{key}", key)
        .replace("{branch}", branch)
        .replace("{base}", base)
}

/// Locates a local checkout of `branch`, trying candidates in priority
/// order and returning the first whose current branch is `branch`:
///
/// 1. The newest (by `started_at`) run row for `key` (any kind) whose
///    recorded worktree directory still exists on disk and is checked out
///    on `branch` — only considered when `run_store` is `Some`.
/// 2. `cwd` itself.
/// 3. `repo_root` itself.
///
/// Returns `None` if none of the above match — the caller then skips
/// auto-rebase entirely and relies on `gh` to fail loudly if the PR turns
/// out to be unmergeable.
fn resolve_checkout_dir(
    git: &dyn GitOps,
    run_store: Option<&RunStore>,
    scope: &str,
    key: &str,
    branch: &str,
    repo_root: &Path,
    cwd: &Path,
) -> Result<Option<PathBuf>, MergeError> {
    if let Some(store) = run_store {
        let mut runs = store.runs_for_ticket(Some(scope), key)?;
        runs.reverse(); // runs_for_ticket is oldest-first; we want newest-first.
        for run in runs {
            if run.worktree.is_empty() {
                continue;
            }
            let dir = PathBuf::from(&run.worktree);
            if dir.exists() && git.current_branch(&dir).ok().as_deref() == Some(branch) {
                return Ok(Some(dir));
            }
        }
    }

    if git.current_branch(cwd).ok().as_deref() == Some(branch) {
        return Ok(Some(cwd.to_path_buf()));
    }

    if git.current_branch(repo_root).ok().as_deref() == Some(branch) {
        return Ok(Some(repo_root.to_path_buf()));
    }

    Ok(None)
}

/// Outcome of [`run_conflict_session`]'s poll loop.
enum ConflictOutcome {
    /// The rebase completed and the working tree is clean and back on
    /// `branch`.
    Resolved,
    /// The rebase is still in progress (or was abandoned) — handed back to
    /// the caller.
    HandedBack,
}

/// Formats `unix_secs` as `YYYY-MM-DD HH:MM:SSZ` (UTC), matching
/// [`crate::work::review_watch`]'s detached-watch log timestamp format.
fn format_ts(unix_secs: i64) -> String {
    // SAFETY: `gmtime` takes a valid `time_t` pointer (a local, non-null
    // stack value) and returns a pointer into a `libc`-owned static `tm`
    // struct, copied out by value below before any other libc call can
    // overwrite it — no reference escapes this function.
    unsafe {
        let t: libc::time_t = unix_secs as libc::time_t;
        let tm_ptr = libc::gmtime(&t);
        if tm_ptr.is_null() {
            return format!("unix:{unix_secs}");
        }
        let tm = *tm_ptr;
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}Z",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}

/// Opens (or reuses) a tmux window running the conflict-resolution agent
/// prompt, then polls until the rebase completes, is abandoned, or times
/// out.
///
/// Reads the prompt (`[work.merge].prompt_file` > `.prompt` > the runner's
/// [`AgentRunner::default_merge_conflict_prompt_template`]) and writes the
/// rendered prompt file before making any tmux call, so a bad `prompt_file`
/// path or a `state_dir` write failure never leaves a half-opened session
/// behind.
///
/// The session is launched the same way an interactive lane run is
/// ([`crate::work::interactive::launch_interactive_run`]'s command
/// construction, inlined here since this session is deliberately
/// untracked — no run row, no [`crate::work::run::PreparedRun`] to hand
/// that helper): [`AgentRunner::build_invocation`] under
/// [`RunMode::Interactive`] with `permission_mode: None` (so it resolves to
/// the runner's own default, `bypassPermissions` for `ClaudeRunner` since
/// issue #29 — never hardcoded here), then
/// [`AgentRunner::tmux_command_line`] renders the command that reads the
/// prompt back from disk. This is what makes the session run unattended
/// instead of stalling on the first permission prompt.
fn run_conflict_session(
    deps: &MergeDeps<'_>,
    key: &str,
    branch: &str,
    base: &str,
    dir: &Path,
    pre_rebase_rev: &str,
    out: &mut dyn Write,
) -> Result<ConflictOutcome, MergeError> {
    let template = match deps.merge_cfg.prompt_file.as_deref() {
        Some(raw) => read_prompt_file(raw, deps.home, "[work.merge].prompt_file")?,
        None => match deps.merge_cfg.prompt.as_deref() {
            Some(prompt) => prompt.to_string(),
            None => deps
                .runner
                .default_merge_conflict_prompt_template()
                .to_string(),
        },
    };
    let prompt = conflict_prompt(&template, key, branch, base);

    let invocation = deps.runner.build_invocation(InvocationInputs {
        prompt,
        model: deps.merge_cfg.model.clone(),
        max_turns: None,
        permission_mode: None,
        settings_path: None,
        run_id: None,
        mode: RunMode::Interactive,
    });
    let prompt_text = deps
        .runner
        .interactive_prompt(&invocation)
        .unwrap_or_default();
    let prompt_path = deps.state_dir.join(format!(
        "merge-{key}-{}.prompt.md",
        deps.clock.now_unix_secs()
    ));
    std::fs::create_dir_all(deps.state_dir).map_err(|source| MergeError::PromptWrite {
        path: deps.state_dir.to_path_buf(),
        source,
    })?;
    std::fs::write(&prompt_path, prompt_text).map_err(|source| MergeError::PromptWrite {
        path: prompt_path.clone(),
        source,
    })?;
    let command = deps.runner.tmux_command_line(&invocation, &prompt_path);

    let current_session = deps.tmux.current_session_name()?;
    let target = current_session
        .clone()
        .unwrap_or_else(|| ticket_session_name(&deps.identity.session_slug(), key));

    let windows = deps.tmux.list_windows()?;
    let existing = session_window_names(&windows, &target);
    let window = unique_window_name(MERGE_WINDOW_BASE, &existing);
    let dir_str = dir.to_string_lossy().into_owned();

    if existing.is_empty() {
        deps.tmux
            .new_session_with_command(&target, &dir_str, &window, &[], &command)?;
    } else {
        deps.tmux
            .new_window_with_command(&target, &window, &dir_str, &[], &command)?;
    }

    let conflicted = deps.git.conflicted_files(dir)?;
    writeln!(
        out,
        "conflicts in {} files; opening agent session (window {target}:{window})",
        conflicted.len()
    )?;
    if current_session.is_none() {
        writeln!(out, "attach: tmux attach -t {target}")?;
    }

    poll_conflict_session(deps, dir, &target, &window, branch, pre_rebase_rev, out)
}

/// The poll loop itself, split out from [`run_conflict_session`] so the
/// window-open step above only ever runs once.
///
/// `pre_rebase_rev` is the branch tip captured before `rebase_onto` ran:
/// `git rebase --abort` restores it exactly, while a completed rebase
/// always rewrites it (a rebase only runs when the base moved), so it is
/// what distinguishes "resolved" from "aborted" once the rebase state
/// clears.
fn poll_conflict_session(
    deps: &MergeDeps<'_>,
    dir: &Path,
    target: &str,
    window: &str,
    branch: &str,
    pre_rebase_rev: &str,
    out: &mut dyn Write,
) -> Result<ConflictOutcome, MergeError> {
    let start = deps.clock.now_unix_secs();
    let mut last_count: Option<usize> = None;
    let mut last_heartbeat = start;

    loop {
        // Resolution is checked before the timeout so a rebase that
        // finished right at the deadline still counts as resolved.
        if !deps.git.rebase_in_progress(dir)? {
            let clean = deps.git.status_is_clean(dir)?;
            let on_branch = deps.git.current_branch(dir).ok().as_deref() == Some(branch);
            if clean && on_branch {
                return match deps.git.rev_parse(dir, branch) {
                    Ok(tip) if tip != pre_rebase_rev => {
                        writeln!(out, "conflicts resolved; rebase complete")?;
                        Ok(ConflictOutcome::Resolved)
                    }
                    _ => {
                        writeln!(out, "rebase was aborted; handing back")?;
                        Ok(ConflictOutcome::HandedBack)
                    }
                };
            }
            writeln!(out, "conflict session ended without completing the rebase")?;
            return Ok(ConflictOutcome::HandedBack);
        }

        let now = deps.clock.now_unix_secs();
        if now - start > CONFLICT_TIMEOUT_SECS {
            writeln!(
                out,
                "conflict resolution timed out after {CONFLICT_TIMEOUT_SECS}s"
            )?;
            writeln!(out, "attach: tmux attach -t {target}")?;
            writeln!(out, "abort: git -C {} rebase --abort", dir.display())?;
            return Ok(ConflictOutcome::HandedBack);
        }

        let windows = deps.tmux.list_windows()?;
        if !has_live_window(&windows, target, window) {
            writeln!(out, "conflict session ended without completing the rebase")?;
            return Ok(ConflictOutcome::HandedBack);
        }

        let conflicted = deps.git.conflicted_files(dir)?;
        let count = conflicted.len();
        let changed = last_count != Some(count);
        let heartbeat_due = now - last_heartbeat >= 60;
        if changed || heartbeat_due {
            writeln!(
                out,
                "[{}] resolving conflicts in {count} files...",
                format_ts(now)
            )?;
            last_heartbeat = now;
        }
        last_count = Some(count);

        deps.sleeper.sleep(CONFLICT_POLL_SECS);
    }
}

/// The whole `tm merge <KEY>` flow. See the module docs for the staged
/// summary.
pub fn run_merge(
    deps: &MergeDeps<'_>,
    key: &str,
    out: &mut dyn Write,
) -> Result<MergeFlowOutcome, MergeError> {
    let scope = deps.identity.scope();

    let repo_root = match deps.run_store {
        Some(store) => {
            resolve_watch_repo_root(Some(&scope), deps.lanes, store, deps.git, deps.cwd, key)?
        }
        None => deps.git.repo_root(deps.cwd)?,
    };

    writeln!(out, "resolving PR for {key}...")?;
    let prs = deps.gh.pr_list(&repo_root)?;
    let pr = find_pr_for_ticket(&prs, key, &|token| deps.jira.is_ticket_key(token)).ok_or_else(
        || MergeError::NoPrForTicket {
            key: key.to_string(),
        },
    )?;
    let number = pr.number;
    let branch = pr.head_ref_name.clone();
    let base = pr.base_ref_name.clone();
    writeln!(out, "found PR #{number} ({branch} -> {base})")?;

    let checkout_dir = resolve_checkout_dir(
        deps.git,
        deps.run_store,
        &scope,
        key,
        &branch,
        &repo_root,
        deps.cwd,
    )?;

    writeln!(out, "fetching origin...")?;
    deps.git
        .fetch_origin(checkout_dir.as_deref().unwrap_or(&repo_root))?;

    match checkout_dir.as_deref() {
        Some(dir) => {
            let origin_branch_ref = format!("origin/{branch}");
            let origin_base_ref = format!("origin/{base}");
            let local_rev = deps.git.rev_parse(dir, &branch)?;
            let remote_rev = deps.git.rev_parse(dir, &origin_branch_ref).map_err(|_| {
                MergeError::RemoteBranchGone {
                    branch: branch.clone(),
                }
            })?;
            // Tracks whether the local tip must be published before the
            // merge: set when local is authoritative (ahead of, or a rebase
            // of, the remote tip) or when a rebase runs below. The pushed
            // tip is what `gh pr merge` merges.
            let mut needs_push = false;
            let mut current_tip = local_rev.clone();
            if local_rev != remote_rev {
                if deps.git.is_ancestor(dir, &branch, &origin_branch_ref)? {
                    deps.git.merge_ff_only(dir, &origin_branch_ref)?;
                    writeln!(out, "fast-forwarded {branch} to {origin_branch_ref}")?;
                    current_tip = remote_rev.clone();
                } else if deps.git.is_ancestor(dir, &origin_branch_ref, &branch)? {
                    writeln!(
                        out,
                        "local {branch} is ahead of {origin_branch_ref}; will push before merging"
                    )?;
                    needs_push = true;
                } else if deps.git.is_ancestor(dir, &origin_base_ref, &branch)? {
                    // A rebase rewrote the local commits (e.g. a prior
                    // handback finished by hand) — local is the rebased
                    // truth and force-with-lease still guards against
                    // anything pushed after our fetch.
                    writeln!(
                        out,
                        "local {branch} diverged from {origin_branch_ref} but is already rebased onto {base}; will push before merging"
                    )?;
                    needs_push = true;
                } else {
                    return Err(MergeError::BranchDiverged {
                        branch: branch.clone(),
                    });
                }
            }

            let rebase_needed = !deps.git.is_ancestor(dir, &origin_base_ref, &branch)?;
            if !rebase_needed {
                writeln!(out, "branch already up to date with {base}")?;
            } else {
                if !deps.git.status_is_clean(dir)? {
                    return Err(MergeError::DirtyWorktree {
                        dir: dir.to_path_buf(),
                    });
                }
                writeln!(out, "rebasing {branch} onto {origin_base_ref}...")?;
                match deps.git.rebase_onto(dir, &origin_base_ref)? {
                    RebaseOutcome::Completed => {
                        writeln!(out, "rebase complete")?;
                        needs_push = true;
                    }
                    RebaseOutcome::Conflicted => {
                        match run_conflict_session(
                            deps,
                            key,
                            &branch,
                            &base,
                            dir,
                            &current_tip,
                            out,
                        )? {
                            ConflictOutcome::Resolved => needs_push = true,
                            ConflictOutcome::HandedBack => {
                                return Ok(MergeFlowOutcome::ConflictsHandedBack);
                            }
                        }
                    }
                }
            }

            if needs_push {
                writeln!(out, "pushing {branch} (force-with-lease)...")?;
                deps.git.push_force_with_lease(dir, &branch)?;
            }
        }
        None => {
            writeln!(
                out,
                "warning: no local checkout of {branch}; skipping auto-rebase"
            )?;
        }
    }

    writeln!(out, "merging PR #{number}...")?;
    deps.gh.pr_merge(&repo_root, number, MERGE_TIMEOUT)?;

    writeln!(out, "syncing local {base}...")?;
    deps.git.fetch_origin(&repo_root)?;
    let repo_root_on_base =
        deps.git.current_branch(&repo_root).ok().as_deref() == Some(base.as_str());
    if repo_root_on_base {
        if deps.git.status_is_clean(&repo_root)? {
            deps.git
                .merge_ff_only(&repo_root, &format!("origin/{base}"))?;
            writeln!(out, "local {base} synced")?;
        } else {
            writeln!(
                out,
                "warning: local {base} checkout is dirty; skipped fast-forward (run: git -C {} merge --ff-only origin/{base})",
                repo_root.display()
            )?;
        }
    } else {
        match deps.git.fetch_branch_to_local(&repo_root, &base) {
            Ok(()) => writeln!(out, "local {base} synced")?,
            Err(err) => writeln!(
                out,
                "warning: failed to sync local {base}: {err} (run: git -C {} fetch origin {base}:{base})",
                repo_root.display()
            )?,
        }
    }

    // --- best-effort cleanup: never fails the flow ---
    let mut skip_branch_due_to_worktree = false;
    if let Some(dir) = checkout_dir.as_deref()
        && dir != repo_root.as_path()
    {
        if deps.cwd.starts_with(dir) {
            writeln!(
                out,
                "warning: skipping worktree removal for {}; cwd is inside it (run: git -C {} worktree remove {} once you leave it)",
                dir.display(),
                repo_root.display(),
                dir.display()
            )?;
            skip_branch_due_to_worktree = true;
        } else if deps.git.is_worktree(dir).unwrap_or(false) {
            match deps.git.remove_worktree(&repo_root, dir) {
                Ok(()) => writeln!(out, "removed worktree {}", dir.display())?,
                Err(err) => writeln!(
                    out,
                    "warning: failed to remove worktree {}: {err} (run: git -C {} worktree remove {})",
                    dir.display(),
                    repo_root.display(),
                    dir.display()
                )?,
            }
        }
    }

    let branch_exists = deps
        .git
        .branch_exists_local(&repo_root, &branch)
        .unwrap_or(false);
    if branch_exists {
        if skip_branch_due_to_worktree {
            writeln!(
                out,
                "warning: skipped deleting local branch {branch}; its worktree removal was skipped (run: git -C {} branch -D {branch} once the worktree is removed)",
                repo_root.display()
            )?;
        } else {
            let local_rev = deps.git.rev_parse(&repo_root, &branch);
            let remote_rev = deps.git.rev_parse(&repo_root, &format!("origin/{branch}"));
            match (local_rev, remote_rev) {
                (Ok(l), Ok(r)) if l == r => match deps.git.delete_branch(&repo_root, &branch) {
                    Ok(()) => writeln!(out, "deleted local branch {branch}")?,
                    Err(err) => writeln!(
                        out,
                        "warning: failed to delete local branch {branch}: {err} (run: git -C {} branch -D {branch})",
                        repo_root.display()
                    )?,
                },
                _ => writeln!(
                    out,
                    "warning: local branch {branch} does not match origin/{branch}; skipped deletion (run: git -C {} branch -D {branch} once verified)",
                    repo_root.display()
                )?,
            }
        }
    }

    if let Some(target_status) = deps.status_on_merge {
        match apply_status_on_merge(deps.jira, key, target_status) {
            StatusTransition::Applied(status) => writeln!(out, "moved {key} to {status}")?,
            StatusTransition::Warning(warning) => writeln!(out, "warning: {warning}")?,
        }
    }

    writeln!(out, "merged PR #{number} for {key}; local {base} synced")?;
    Ok(MergeFlowOutcome::Merged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::claude::ClaudeRunner;
    use crate::github::gh_cli::FakeGhCli;
    use crate::github::pr::PrInfo;
    use crate::jira::fake::FakeJiraClient;
    use crate::runs::{RunStore, StartRun};
    use crate::ticketing::types::{Issue, IssueFields, Status, StatusCategory};
    use crate::work::git::FakeGitOps;
    use crate::work::review_watch::{FakeClock, FakeSleeper};
    use crate::work::tmux::{FakeTmuxOps, TmuxCall, TmuxWindow};
    use tempfile::tempdir;

    fn identity() -> BackendIdentity {
        BackendIdentity::Jira {
            base_url: "https://x.atlassian.net".to_string(),
            project_key: "PROJ".to_string(),
        }
    }

    fn pr_info(number: u64, branch: &str, base: &str, key: &str) -> PrInfo {
        PrInfo {
            number,
            url: format!("https://github.com/example/repo/pull/{number}"),
            title: format!("[{key}] fix the thing"),
            body: String::new(),
            head_ref_name: branch.to_string(),
            base_ref_name: base.to_string(),
        }
    }

    fn issue_with_status(key: &str, status: &str) -> Issue {
        Issue {
            key: key.to_string(),
            fields: IssueFields {
                summary: "Fix the thing".to_string(),
                status: Status {
                    name: status.to_string(),
                    status_category: StatusCategory {
                        key: "done".to_string(),
                    },
                },
                description: None,
                assignee: None,
                issue_links: vec![],
            },
        }
    }

    fn open_store(dir: &Path) -> RunStore {
        RunStore::open(&dir.join("runs.db")).expect("open should succeed")
    }

    struct Fixture {
        _db_dir: tempfile::TempDir,
        store: RunStore,
        git: FakeGitOps,
        gh: FakeGhCli,
        tmux: FakeTmuxOps,
        jira: FakeJiraClient,
        runner: ClaudeRunner,
        clock: FakeClock,
        sleeper: FakeSleeper,
        cwd: PathBuf,
        home: PathBuf,
        identity: BackendIdentity,
        lanes: BTreeMap<String, LaneConfig>,
        merge_cfg: MergeConfig,
        status_on_merge: Option<String>,
        _worktree_dirs: Vec<tempfile::TempDir>,
        _state_dir: tempfile::TempDir,
        state_dir: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let db_dir = tempdir().unwrap();
            let store = open_store(db_dir.path());
            let state_tmp = tempdir().unwrap();
            let state_dir = state_tmp.path().join("state");
            Fixture {
                _db_dir: db_dir,
                store,
                git: FakeGitOps::new()
                    .with_repo_root(Ok(PathBuf::from("/repo")))
                    .with_current_branch(Err(GitError::Command {
                        command: "git branch --show-current".to_string(),
                        exit_code: Some(1),
                        stderr: "no checkout".to_string(),
                    })),
                gh: FakeGhCli::new(),
                tmux: FakeTmuxOps::new(),
                jira: FakeJiraClient::new(),
                runner: ClaudeRunner,
                clock: FakeClock::at(1_000),
                sleeper: FakeSleeper::default(),
                cwd: PathBuf::from("/repo"),
                home: PathBuf::from("/home/jowi"),
                identity: identity(),
                lanes: BTreeMap::new(),
                merge_cfg: MergeConfig::default(),
                status_on_merge: None,
                _worktree_dirs: Vec::new(),
                _state_dir: state_tmp,
                state_dir,
            }
        }

        /// Registers a `lane`-kind run row for `PROJ-1` at a fresh temp
        /// directory checked out on `branch`, and configures the fake to
        /// report that branch for the directory — the checkout-resolution
        /// candidate most tests that need a real rebase to run exercise.
        /// Keeps `repo_root` ("/repo", a path that doesn't exist on disk)
        /// free to represent the base branch's checkout in the same test.
        fn register_checkout(&mut self, branch: &str) -> PathBuf {
            let dir = tempdir().unwrap();
            let path = dir.path().to_path_buf();
            self.store
                .start_run(&StartRun {
                    scope: self.identity.scope(),
                    ticket: "PROJ-1".to_string(),
                    lane: "lane".to_string(),
                    worktree: path.to_string_lossy().into_owned(),
                    branch: Some(branch.to_string()),
                    pid: None,
                    kind: "lane".to_string(),
                    log_path: None,
                })
                .unwrap();
            self.git = std::mem::replace(&mut self.git, FakeGitOps::new())
                .with_current_branch_for(path.clone(), Ok(branch.to_string()));
            self._worktree_dirs.push(dir);
            path
        }

        fn deps(&self) -> MergeDeps<'_> {
            MergeDeps {
                git: &self.git,
                gh: &self.gh,
                tmux: &self.tmux,
                jira: &self.jira,
                runner: &self.runner,
                run_store: Some(&self.store),
                clock: &self.clock,
                sleeper: &self.sleeper,
                cwd: &self.cwd,
                home: &self.home,
                identity: &self.identity,
                lanes: &self.lanes,
                merge_cfg: &self.merge_cfg,
                status_on_merge: self.status_on_merge.as_deref(),
                state_dir: &self.state_dir,
            }
        }
    }

    fn run(deps: &MergeDeps<'_>, key: &str) -> (Result<MergeFlowOutcome, MergeError>, String) {
        let mut out = Vec::new();
        let result = run_merge(deps, key, &mut out);
        (result, String::from_utf8(out).unwrap())
    }

    // --- conflict_prompt ---

    #[test]
    fn conflict_prompt_substitutes_every_placeholder() {
        let rendered = conflict_prompt(
            "resolve {key} on {branch} onto {base}",
            "PROJ-1",
            "proj-1-fix",
            "main",
        );
        assert_eq!(rendered, "resolve PROJ-1 on proj-1-fix onto main");
    }

    // --- happy path, no rebase needed ---

    #[test]
    fn no_rebase_needed_merges_and_syncs_base_without_pushing() {
        let mut fx = Fixture::new();
        fx.gh = fx
            .gh
            .with_pr_list(Ok(vec![pr_info(7, "proj-1-fix", "main", "PROJ-1")]));
        fx.git = fx
            .git
            .with_current_branch(Ok("main".to_string()))
            .with_current_branch_for(PathBuf::from("/repo"), Ok("main".to_string()))
            .with_rev_parse_result("proj-1-fix", Ok("aaa".to_string()))
            .with_rev_parse_result("origin/proj-1-fix", Ok("aaa".to_string()))
            .with_is_ancestor_result(Ok(true))
            .with_branch_exists_local(Ok(false));

        let (result, _out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)));
        assert!(
            fx.git.rebase_onto_calls().is_empty(),
            "no rebase should have run"
        );
        assert!(
            fx.git.push_force_with_lease_calls().is_empty(),
            "no push should have happened"
        );
        assert_eq!(fx.gh.pr_merge_calls(), vec![(PathBuf::from("/repo"), 7)]);
        assert_eq!(
            fx.git.merge_ff_only_calls(),
            vec![(PathBuf::from("/repo"), "origin/main".to_string())],
            "base should be fast-forwarded once, at cleanup time"
        );
    }

    // --- rebase needed, completes clean ---

    #[test]
    fn rebase_needed_completes_clean_then_pushes_merges_and_syncs_in_order() {
        let mut fx = Fixture::new();
        fx.register_checkout("proj-1-fix");
        fx.gh = fx
            .gh
            .with_pr_list(Ok(vec![pr_info(7, "proj-1-fix", "main", "PROJ-1")]));
        fx.git = fx
            .git
            .with_current_branch(Ok("main".to_string()))
            .with_rev_parse_result("proj-1-fix", Ok("aaa".to_string()))
            .with_rev_parse_result("origin/proj-1-fix", Ok("aaa".to_string()))
            .with_is_ancestor_result(Ok(false))
            .with_rebase_onto_result(Ok(RebaseOutcome::Completed))
            .with_branch_exists_local(Ok(false));

        let (result, _out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)));
        assert_eq!(
            fx.git.call_log(),
            vec![
                "fetch_origin",
                "rebase_onto",
                "push_force_with_lease",
                "fetch_origin",
                "merge_ff_only",
            ]
        );
        assert_eq!(fx.gh.pr_merge_calls(), vec![(PathBuf::from("/repo"), 7)]);
    }

    // --- conflict: current session ---

    #[test]
    fn conflict_opens_a_window_on_the_current_session_when_inside_tmux() {
        let mut fx = Fixture::new();
        fx.gh = fx
            .gh
            .with_pr_list(Ok(vec![pr_info(7, "proj-1-fix", "main", "PROJ-1")]));
        fx.git = fx
            .git
            .with_current_branch(Ok("proj-1-fix".to_string()))
            .with_current_branch_for(PathBuf::from("/repo"), Ok("proj-1-fix".to_string()))
            // The completed rebase rewrites the tip: "aaa" before, "bbb"
            // once the conflict session finishes.
            .with_rev_parse_sequence(
                "proj-1-fix",
                vec![Ok("aaa".to_string()), Ok("bbb".to_string())],
            )
            .with_rev_parse_result("origin/proj-1-fix", Ok("aaa".to_string()))
            .with_is_ancestor_result(Ok(false))
            .with_rebase_onto_result(Ok(RebaseOutcome::Conflicted))
            .with_rebase_in_progress_sequence(vec![Ok(true), Ok(true), Ok(false)])
            .with_conflicted_files_sequence(vec![
                Ok(vec!["a.rs".to_string()]),
                Ok(vec!["a.rs".to_string()]),
            ])
            .with_branch_exists_local(Ok(false));
        // After the conflict resolves, the checkout is back on the branch
        // and clean.
        fx.git = fx.git.with_current_branch(Ok("proj-1-fix".to_string()));
        fx.tmux = fx
            .tmux
            .with_current_session_name(Ok(Some("dev".to_string())))
            .with_list_windows_sequence(vec![
                // Snapshot taken before the window is opened: "dev" already
                // has a window (we're inside it), so the merge window is
                // appended, not created as a new session.
                Ok(vec![TmuxWindow {
                    session: "dev".to_string(),
                    name: "shell".to_string(),
                    dead: false,
                }]),
                // Snapshot(s) taken by the poll loop's liveness check, once
                // the merge window exists.
                Ok(vec![
                    TmuxWindow {
                        session: "dev".to_string(),
                        name: "shell".to_string(),
                        dead: false,
                    },
                    TmuxWindow {
                        session: "dev".to_string(),
                        name: "merge".to_string(),
                        dead: false,
                    },
                ]),
            ]);

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)));
        let calls = fx.tmux.calls();
        assert!(
            calls
                .iter()
                .any(|c| matches!(c, TmuxCall::NewWindowWithCommand { name, .. } if name == "dev")),
            "expected a window opened on the current session, got: {calls:?}"
        );
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, TmuxCall::NewSessionWithCommand { .. })),
            "must not create a new session when already inside tmux"
        );
        assert!(
            !out.contains("attach:"),
            "no attach hint needed inside tmux"
        );
        assert_eq!(fx.git.push_force_with_lease_calls().len(), 1);
        assert_eq!(fx.gh.pr_merge_calls().len(), 1);
    }

    // --- conflict: outside tmux ---

    #[test]
    fn conflict_outside_tmux_creates_a_new_ticket_session_with_attach_hint() {
        let mut fx = Fixture::new();
        fx.gh = fx
            .gh
            .with_pr_list(Ok(vec![pr_info(7, "proj-1-fix", "main", "PROJ-1")]));
        fx.git = fx
            .git
            .with_current_branch(Ok("proj-1-fix".to_string()))
            .with_current_branch_for(PathBuf::from("/repo"), Ok("proj-1-fix".to_string()))
            // The completed rebase rewrites the tip: "aaa" before, "bbb"
            // once the conflict session finishes.
            .with_rev_parse_sequence(
                "proj-1-fix",
                vec![Ok("aaa".to_string()), Ok("bbb".to_string())],
            )
            .with_rev_parse_result("origin/proj-1-fix", Ok("aaa".to_string()))
            .with_is_ancestor_result(Ok(false))
            .with_rebase_onto_result(Ok(RebaseOutcome::Conflicted))
            .with_rebase_in_progress_sequence(vec![Ok(false)])
            .with_branch_exists_local(Ok(false));
        // No current session: `current_session_name_result` defaults to
        // `Ok(None)`.

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)));
        let calls = fx.tmux.calls();
        assert!(
            calls.iter().any(|c| matches!(
                c,
                TmuxCall::NewSessionWithCommand { name, .. } if name == "tm-proj-proj-1"
            )),
            "expected a new session named after the ticket, got: {calls:?}"
        );
        assert!(out.contains("attach: tmux attach -t tm-proj-proj-1"));
    }

    // --- conflict: window dies mid-rebase ---

    #[test]
    fn conflict_window_dying_mid_rebase_hands_back_without_pushing_or_merging() {
        let mut fx = Fixture::new();
        fx.gh = fx
            .gh
            .with_pr_list(Ok(vec![pr_info(7, "proj-1-fix", "main", "PROJ-1")]));
        fx.git = fx
            .git
            .with_current_branch(Ok("proj-1-fix".to_string()))
            .with_current_branch_for(PathBuf::from("/repo"), Ok("proj-1-fix".to_string()))
            .with_rev_parse_result("proj-1-fix", Ok("aaa".to_string()))
            .with_rev_parse_result("origin/proj-1-fix", Ok("aaa".to_string()))
            .with_is_ancestor_result(Ok(false))
            .with_rebase_onto_result(Ok(RebaseOutcome::Conflicted))
            .with_rebase_in_progress_sequence(vec![Ok(true)])
            .with_conflicted_files_sequence(vec![Ok(vec!["a.rs".to_string()])]);
        // list_windows returns no windows at all once queried inside the
        // poll loop, so the just-opened window reads as dead.
        fx.tmux = fx
            .tmux
            .with_current_session_name(Ok(Some("dev".to_string())))
            .with_list_windows(Ok(vec![]));

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::ConflictsHandedBack)));
        assert!(out.contains("conflict session ended without completing the rebase"));
        assert!(fx.git.push_force_with_lease_calls().is_empty());
        assert!(fx.gh.pr_merge_calls().is_empty());
    }

    // --- conflict: timeout ---

    #[test]
    fn conflict_timeout_hands_back_with_hints() {
        let mut fx = Fixture::new();
        fx.gh = fx
            .gh
            .with_pr_list(Ok(vec![pr_info(7, "proj-1-fix", "main", "PROJ-1")]));
        fx.git = fx
            .git
            .with_current_branch(Ok("proj-1-fix".to_string()))
            .with_current_branch_for(PathBuf::from("/repo"), Ok("proj-1-fix".to_string()))
            .with_rev_parse_result("proj-1-fix", Ok("aaa".to_string()))
            .with_rev_parse_result("origin/proj-1-fix", Ok("aaa".to_string()))
            .with_is_ancestor_result(Ok(false))
            .with_rebase_onto_result(Ok(RebaseOutcome::Conflicted))
            .with_rebase_in_progress_sequence(vec![Ok(true)])
            .with_conflicted_files_sequence(vec![Ok(vec!["a.rs".to_string()])]);
        fx.tmux = fx
            .tmux
            .with_current_session_name(Ok(Some("dev".to_string())))
            .with_list_windows(Ok(vec![TmuxWindow {
                session: "dev".to_string(),
                name: "merge".to_string(),
                dead: false,
            }]));
        fx.clock = FakeClock::advancing(1_000, 901);

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::ConflictsHandedBack)));
        assert!(out.contains("timed out after 900s"));
        assert!(out.contains("attach: tmux attach -t dev"));
        assert!(out.contains("rebase --abort"));
    }

    // --- dirty worktree ---

    #[test]
    fn dirty_worktree_errors_before_any_rebase_call() {
        let mut fx = Fixture::new();
        fx.gh = fx
            .gh
            .with_pr_list(Ok(vec![pr_info(7, "proj-1-fix", "main", "PROJ-1")]));
        fx.git = fx
            .git
            .with_current_branch(Ok("proj-1-fix".to_string()))
            .with_current_branch_for(PathBuf::from("/repo"), Ok("proj-1-fix".to_string()))
            .with_rev_parse_result("proj-1-fix", Ok("aaa".to_string()))
            .with_rev_parse_result("origin/proj-1-fix", Ok("aaa".to_string()))
            .with_is_ancestor_result(Ok(false))
            .with_status_is_clean(Ok(false));

        let (result, _out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Err(MergeError::DirtyWorktree { .. })));
        assert!(fx.git.rebase_onto_calls().is_empty());
        assert!(fx.gh.pr_merge_calls().is_empty());
    }

    // --- diverged branch ---

    #[test]
    fn diverged_branch_errors_before_any_rebase_call() {
        let mut fx = Fixture::new();
        fx.gh = fx
            .gh
            .with_pr_list(Ok(vec![pr_info(7, "proj-1-fix", "main", "PROJ-1")]));
        fx.git = fx
            .git
            .with_current_branch(Ok("proj-1-fix".to_string()))
            .with_current_branch_for(PathBuf::from("/repo"), Ok("proj-1-fix".to_string()))
            .with_rev_parse_result("proj-1-fix", Ok("aaa".to_string()))
            .with_rev_parse_result("origin/proj-1-fix", Ok("bbb".to_string()))
            .with_is_ancestor_result(Ok(false));

        let (result, _out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Err(MergeError::BranchDiverged { .. })));
        assert!(fx.git.rebase_onto_calls().is_empty());
    }

    // --- local ahead of remote: unpushed local commits are authoritative ---

    #[test]
    fn local_ahead_of_remote_is_pushed_before_merging() {
        let mut fx = Fixture::new();
        let checkout = fx.register_checkout("proj-1-fix");
        fx.gh = fx
            .gh
            .with_pr_list(Ok(vec![pr_info(7, "proj-1-fix", "main", "PROJ-1")]));
        fx.git = fx
            .git
            .with_current_branch(Ok("main".to_string()))
            .with_rev_parse_result("proj-1-fix", Ok("bbb".to_string()))
            .with_rev_parse_result("origin/proj-1-fix", Ok("aaa".to_string()))
            .with_is_ancestor_result(Ok(false))
            // remote is an ancestor of local: local is strictly ahead.
            .with_is_ancestor_for("origin/proj-1-fix", "proj-1-fix", Ok(true))
            // local already contains the latest base: no rebase needed.
            .with_is_ancestor_for("origin/main", "proj-1-fix", Ok(true))
            .with_branch_exists_local(Ok(false));

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)));
        assert!(
            fx.git.rebase_onto_calls().is_empty(),
            "up-to-date-with-base branch must not be rebased"
        );
        assert_eq!(
            fx.git.push_force_with_lease_calls(),
            vec![(checkout, "proj-1-fix".to_string())],
            "the unpushed local commits must be published before the merge"
        );
        assert_eq!(fx.gh.pr_merge_calls().len(), 1);
        assert!(out.contains("ahead of origin/proj-1-fix"));
    }

    // --- diverged, but local is already rebased onto the base ---
    // (the state a handback or a manual `git rebase --continue` leaves
    // behind — rerunning `tm merge` must pick it up, not dead-end)

    #[test]
    fn diverged_but_rebased_local_is_pushed_before_merging() {
        let mut fx = Fixture::new();
        let checkout = fx.register_checkout("proj-1-fix");
        fx.gh = fx
            .gh
            .with_pr_list(Ok(vec![pr_info(7, "proj-1-fix", "main", "PROJ-1")]));
        fx.git = fx
            .git
            .with_current_branch(Ok("main".to_string()))
            .with_rev_parse_result("proj-1-fix", Ok("bbb".to_string()))
            .with_rev_parse_result("origin/proj-1-fix", Ok("aaa".to_string()))
            .with_is_ancestor_result(Ok(false))
            // Neither tip is an ancestor of the other (a rebase rewrote the
            // local commits), but local already sits on the latest base.
            .with_is_ancestor_for("origin/main", "proj-1-fix", Ok(true))
            .with_branch_exists_local(Ok(false));

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)));
        assert!(fx.git.rebase_onto_calls().is_empty());
        assert_eq!(
            fx.git.push_force_with_lease_calls(),
            vec![(checkout, "proj-1-fix".to_string())]
        );
        assert_eq!(fx.gh.pr_merge_calls().len(), 1);
        assert!(out.contains("already rebased onto main"));
    }

    // --- conflict session aborted the rebase ---

    #[test]
    fn aborted_rebase_hands_back_without_pushing_or_merging() {
        let mut fx = Fixture::new();
        fx.register_checkout("proj-1-fix");
        fx.gh = fx
            .gh
            .with_pr_list(Ok(vec![pr_info(7, "proj-1-fix", "main", "PROJ-1")]));
        fx.git = fx
            .git
            .with_current_branch(Ok("proj-1-fix".to_string()))
            // `git rebase --abort` restores the pre-rebase tip exactly, so
            // the branch rev never changes across the whole flow.
            .with_rev_parse_result("proj-1-fix", Ok("aaa".to_string()))
            .with_rev_parse_result("origin/proj-1-fix", Ok("aaa".to_string()))
            .with_is_ancestor_result(Ok(false))
            .with_rebase_onto_result(Ok(RebaseOutcome::Conflicted))
            .with_rebase_in_progress_sequence(vec![Ok(false)])
            .with_branch_exists_local(Ok(false));
        fx.tmux = fx
            .tmux
            .with_current_session_name(Ok(Some("dev".to_string())))
            .with_list_windows(Ok(vec![TmuxWindow {
                session: "dev".to_string(),
                name: "shell".to_string(),
                dead: false,
            }]));

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::ConflictsHandedBack)));
        assert!(
            out.contains("rebase was aborted"),
            "an abort must not read as success; got: {out}"
        );
        assert!(fx.git.push_force_with_lease_calls().is_empty());
        assert!(fx.gh.pr_merge_calls().is_empty());
    }

    // --- local behind remote: catch-up then normal flow ---

    #[test]
    fn local_behind_remote_catches_up_via_ff_only_then_proceeds() {
        let mut fx = Fixture::new();
        let checkout = fx.register_checkout("proj-1-fix");
        fx.gh = fx
            .gh
            .with_pr_list(Ok(vec![pr_info(7, "proj-1-fix", "main", "PROJ-1")]));
        fx.git = fx
            .git
            .with_current_branch(Ok("main".to_string()))
            .with_rev_parse_result("proj-1-fix", Ok("aaa".to_string()))
            .with_rev_parse_result("origin/proj-1-fix", Ok("bbb".to_string()))
            .with_is_ancestor_result(Ok(true))
            .with_branch_exists_local(Ok(false));

        let (result, _out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)));
        assert_eq!(
            fx.git.merge_ff_only_calls(),
            vec![
                (checkout, "origin/proj-1-fix".to_string()),
                (PathBuf::from("/repo"), "origin/main".to_string()),
            ]
        );
        assert!(
            fx.git.rebase_onto_calls().is_empty(),
            "catch-up alone must not run a rebase (is_ancestor stays true for the base check too)"
        );
        assert!(
            fx.git.push_force_with_lease_calls().is_empty(),
            "a catch-up alone (no rebase) must not push"
        );
    }

    // --- no local checkout ---

    #[test]
    fn no_local_checkout_warns_and_skips_rebase_but_still_merges() {
        let mut fx = Fixture::new();
        fx.gh = fx
            .gh
            .with_pr_list(Ok(vec![pr_info(7, "proj-1-fix", "main", "PROJ-1")]));
        // current_branch never matches "proj-1-fix" anywhere.
        fx.git = fx.git.with_branch_exists_local(Ok(false));

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)));
        assert!(out.contains("no local checkout of proj-1-fix; skipping auto-rebase"));
        assert!(fx.git.rebase_onto_calls().is_empty());
        assert_eq!(fx.gh.pr_merge_calls(), vec![(PathBuf::from("/repo"), 7)]);
    }

    // --- base not checked out at repo_root ---

    #[test]
    fn base_not_checked_out_at_repo_root_uses_fetch_branch_to_local() {
        let mut fx = Fixture::new();
        fx.gh = fx
            .gh
            .with_pr_list(Ok(vec![pr_info(7, "proj-1-fix", "main", "PROJ-1")]));
        fx.git = fx
            .git
            .with_current_branch(Ok("some-other-branch".to_string()))
            .with_branch_exists_local(Ok(false));

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)));
        assert_eq!(
            fx.git.fetch_branch_to_local_calls(),
            vec![(PathBuf::from("/repo"), "main".to_string())]
        );
        assert!(out.contains("local main synced"));
    }

    #[test]
    fn base_fetch_branch_to_local_failure_only_warns() {
        let mut fx = Fixture::new();
        fx.gh = fx
            .gh
            .with_pr_list(Ok(vec![pr_info(7, "proj-1-fix", "main", "PROJ-1")]));
        fx.git = fx
            .git
            .with_current_branch(Ok("some-other-branch".to_string()))
            .with_branch_exists_local(Ok(false))
            .with_fetch_branch_to_local_result(Err(GitError::Command {
                command: "git fetch".to_string(),
                exit_code: Some(1),
                stderr: "branch checked out elsewhere".to_string(),
            }));

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(
            matches!(result, Ok(MergeFlowOutcome::Merged)),
            "a base-sync failure must only warn, not fail the flow"
        );
        assert!(out.contains("warning: failed to sync local main"));
    }

    // --- dirty base checkout ---

    #[test]
    fn dirty_base_checkout_skips_ff_with_warning() {
        let mut fx = Fixture::new();
        fx.gh = fx
            .gh
            .with_pr_list(Ok(vec![pr_info(7, "proj-1-fix", "main", "PROJ-1")]));
        fx.git = fx
            .git
            .with_current_branch(Ok("main".to_string()))
            .with_status_is_clean(Ok(false))
            .with_branch_exists_local(Ok(false));

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)));
        assert!(fx.git.merge_ff_only_calls().is_empty());
        assert!(out.contains("warning: local main checkout is dirty"));
    }

    // --- cleanup: cwd inside worktree ---

    #[test]
    fn cwd_inside_worktree_skips_removal_and_branch_deletion_with_hints() {
        let mut fx = Fixture::new();
        let wt_dir = tempdir().unwrap();
        let run_id = fx
            .store
            .start_run(&StartRun {
                scope: fx.identity.scope(),
                ticket: "PROJ-1".to_string(),
                lane: "lane".to_string(),
                worktree: wt_dir.path().to_string_lossy().into_owned(),
                branch: Some("proj-1-fix".to_string()),
                pid: None,
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();
        assert!(run_id > 0);

        fx.gh = fx
            .gh
            .with_pr_list(Ok(vec![pr_info(7, "proj-1-fix", "main", "PROJ-1")]));
        fx.git = fx
            .git
            .with_current_branch_for(wt_dir.path().to_path_buf(), Ok("proj-1-fix".to_string()))
            .with_current_branch_for(PathBuf::from("/repo"), Ok("main".to_string()))
            .with_rev_parse_result("proj-1-fix", Ok("aaa".to_string()))
            .with_rev_parse_result("origin/proj-1-fix", Ok("aaa".to_string()))
            .with_is_ancestor_result(Ok(true))
            .with_branch_exists_local(Ok(true));
        // cwd is a subdirectory inside the worktree.
        fx.cwd = wt_dir.path().join("src");

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)));
        assert!(fx.git.remove_worktree_calls().is_empty());
        assert!(fx.git.delete_branch_calls().is_empty());
        assert!(out.contains("skipping worktree removal"));
        assert!(out.contains("skipped deleting local branch proj-1-fix"));
    }

    // --- cleanup: rev mismatch skips branch deletion ---

    #[test]
    fn branch_tip_mismatch_with_origin_skips_deletion() {
        let mut fx = Fixture::new();
        fx.gh = fx
            .gh
            .with_pr_list(Ok(vec![pr_info(7, "proj-1-fix", "main", "PROJ-1")]));
        fx.git = fx
            .git
            .with_current_branch(Ok("main".to_string()))
            .with_branch_exists_local(Ok(true))
            .with_rev_parse_result("proj-1-fix", Ok("aaa".to_string()))
            .with_rev_parse_result("origin/proj-1-fix", Ok("bbb".to_string()));

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)));
        assert!(fx.git.delete_branch_calls().is_empty());
        assert!(out.contains("skipped deletion"));
    }

    // --- status_on_merge ---

    #[test]
    fn status_on_merge_applied_prints_the_new_status() {
        let mut fx = Fixture::new();
        fx.gh = fx
            .gh
            .with_pr_list(Ok(vec![pr_info(7, "proj-1-fix", "main", "PROJ-1")]));
        fx.git = fx
            .git
            .with_current_branch(Ok("main".to_string()))
            .with_branch_exists_local(Ok(false));
        fx.status_on_merge = Some("Done".to_string());
        fx.jira = fx
            .jira
            .with_issue("PROJ-1", issue_with_status("PROJ-1", "Done"));

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)));
        assert!(out.contains("moved PROJ-1 to Done"));
    }

    // --- no PR for ticket ---

    #[test]
    fn no_pr_for_ticket_errors() {
        let fx = Fixture::new();

        let (result, _out) = run(&fx.deps(), "PROJ-9");

        assert!(matches!(result, Err(MergeError::NoPrForTicket { .. })));
    }

    // --- prompt precedence ---

    #[test]
    fn prompt_file_takes_precedence_over_prompt_and_default() {
        let mut fx = Fixture::new();
        let prompt_dir = tempdir().unwrap();
        let prompt_path = prompt_dir.path().join("merge.md");
        std::fs::write(&prompt_path, "custom {key} {branch} {base} prompt").unwrap();
        fx.merge_cfg.prompt_file = Some(prompt_path.to_string_lossy().into_owned());
        fx.merge_cfg.prompt = Some("should not be used".to_string());

        fx.gh = fx
            .gh
            .with_pr_list(Ok(vec![pr_info(7, "proj-1-fix", "main", "PROJ-1")]));
        fx.git = fx
            .git
            .with_current_branch(Ok("proj-1-fix".to_string()))
            .with_current_branch_for(PathBuf::from("/repo"), Ok("proj-1-fix".to_string()))
            .with_rev_parse_result("proj-1-fix", Ok("aaa".to_string()))
            .with_rev_parse_result("origin/proj-1-fix", Ok("aaa".to_string()))
            .with_is_ancestor_result(Ok(false))
            .with_rebase_onto_result(Ok(RebaseOutcome::Conflicted))
            .with_rebase_in_progress_sequence(vec![Ok(false)])
            .with_branch_exists_local(Ok(false));
        fx.tmux = fx
            .tmux
            .with_current_session_name(Ok(Some("dev".to_string())))
            .with_list_windows(Ok(vec![TmuxWindow {
                session: "dev".to_string(),
                name: "shell".to_string(),
                dead: false,
            }]));

        let _ = run(&fx.deps(), "PROJ-1");

        let calls = fx.tmux.calls();
        let command = calls
            .iter()
            .find_map(|c| match c {
                TmuxCall::NewWindowWithCommand { command, .. } => Some(command.clone()),
                _ => None,
            })
            .expect("a conflict window should have been opened");

        let prompt_path = fx.state_dir.join("merge-PROJ-1-1000.prompt.md");
        assert!(
            command.contains(&format!("\"$(cat '{}')\"", prompt_path.display())),
            "expected the command to read the prompt back from {}, got: {command}",
            prompt_path.display()
        );
        let written = std::fs::read_to_string(&prompt_path).expect("prompt file should exist");
        assert_eq!(
            written, "custom PROJ-1 proj-1-fix main prompt",
            "expected the prompt_file contents (substituted) written to the prompt file"
        );
    }

    #[test]
    fn prompt_file_error_propagates_before_any_tmux_mutation() {
        let mut fx = Fixture::new();
        fx.merge_cfg.prompt_file = Some("/does/not/exist.md".to_string());

        fx.gh = fx
            .gh
            .with_pr_list(Ok(vec![pr_info(7, "proj-1-fix", "main", "PROJ-1")]));
        fx.git = fx
            .git
            .with_current_branch(Ok("proj-1-fix".to_string()))
            .with_current_branch_for(PathBuf::from("/repo"), Ok("proj-1-fix".to_string()))
            .with_rev_parse_result("proj-1-fix", Ok("aaa".to_string()))
            .with_rev_parse_result("origin/proj-1-fix", Ok("aaa".to_string()))
            .with_is_ancestor_result(Ok(false))
            .with_rebase_onto_result(Ok(RebaseOutcome::Conflicted));

        let (result, _out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Err(MergeError::PromptFile(_))));
        assert!(
            fx.tmux.calls().is_empty(),
            "a bad prompt_file must fail before any tmux call, got: {:?}",
            fx.tmux.calls()
        );
    }

    /// Sets up a fixture whose merge flow reaches a conflicted rebase for
    /// `PROJ-1` on branch `proj-1-fix` (base `main`), matching the shared
    /// preconditions `prompt_file_takes_precedence_over_prompt_and_default`
    /// and the tests below need to reach [`run_conflict_session`].
    fn conflicted_fixture() -> Fixture {
        let mut fx = Fixture::new();
        fx.gh = fx
            .gh
            .with_pr_list(Ok(vec![pr_info(7, "proj-1-fix", "main", "PROJ-1")]));
        fx.git = fx
            .git
            .with_current_branch(Ok("proj-1-fix".to_string()))
            .with_current_branch_for(PathBuf::from("/repo"), Ok("proj-1-fix".to_string()))
            .with_rev_parse_result("proj-1-fix", Ok("aaa".to_string()))
            .with_rev_parse_result("origin/proj-1-fix", Ok("aaa".to_string()))
            .with_is_ancestor_result(Ok(false))
            .with_rebase_onto_result(Ok(RebaseOutcome::Conflicted))
            .with_rebase_in_progress_sequence(vec![Ok(false)])
            .with_branch_exists_local(Ok(false));
        fx.tmux = fx
            .tmux
            .with_current_session_name(Ok(Some("dev".to_string())))
            .with_list_windows(Ok(vec![TmuxWindow {
                session: "dev".to_string(),
                name: "shell".to_string(),
                dead: false,
            }]));
        fx
    }

    fn launched_command(fx: &Fixture) -> String {
        fx.tmux
            .calls()
            .iter()
            .find_map(|c| match c {
                TmuxCall::NewWindowWithCommand { command, .. } => Some(command.clone()),
                _ => None,
            })
            .expect("a conflict window should have been opened")
    }

    #[test]
    fn conflict_session_defaults_to_bypass_permissions() {
        let fx = conflicted_fixture();

        let _ = run(&fx.deps(), "PROJ-1");

        let command = launched_command(&fx);
        assert!(
            command.contains("'--permission-mode' 'bypassPermissions'"),
            "expected an unattended default permission mode, got: {command}"
        );
    }

    #[test]
    fn conflict_session_strips_billing_env_vars() {
        let fx = conflicted_fixture();

        let _ = run(&fx.deps(), "PROJ-1");

        let command = launched_command(&fx);
        assert!(
            command.starts_with(
                "env -u ANTHROPIC_API_KEY -u ANTHROPIC_AUTH_TOKEN -u CLAUDECODE claude "
            ),
            "expected the billing-safety env -u prefix, got: {command}"
        );
    }

    #[test]
    fn conflict_session_model_becomes_model_flag() {
        let mut fx = conflicted_fixture();
        fx.merge_cfg.model = Some("opus".to_string());

        let _ = run(&fx.deps(), "PROJ-1");

        let command = launched_command(&fx);
        assert!(
            command.contains("'--model' 'opus'"),
            "expected [work.merge].model to land as --model, got: {command}"
        );
    }

    #[test]
    fn conflict_session_prompt_write_failure_creates_no_window() {
        let fx = conflicted_fixture();
        // A state_dir path that is itself an existing file can never be
        // created as a directory, so writing the prompt file underneath it
        // fails deterministically.
        let blocked = fx.state_dir.clone();
        std::fs::create_dir_all(blocked.parent().unwrap()).unwrap();
        std::fs::write(&blocked, b"not a directory").unwrap();

        let (result, _out) = run(&fx.deps(), "PROJ-1");

        assert!(
            matches!(result, Err(MergeError::PromptWrite { .. })),
            "expected a PromptWrite error, got: {result:?}"
        );
        assert!(
            fx.tmux.calls().iter().all(|c| !matches!(
                c,
                TmuxCall::NewWindowWithCommand { .. } | TmuxCall::NewSessionWithCommand { .. }
            )),
            "a prompt-file write failure must create zero tmux windows, got: {:?}",
            fx.tmux.calls()
        );
    }
}
