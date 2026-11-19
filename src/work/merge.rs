//! `tm merge`: rebase a ticket's PR branch onto its base, hand conflicts to
//! an interactive agent session when the rebase can't finish cleanly, then
//! merge the PR and clean up.
//!
//! [`run_merge`] is the whole flow, staged roughly as: resolve the ticket's
//! open PR, locate a local checkout of its branch (if any), fetch and
//! rebase (opening a conflict-resolution tmux session and polling it to
//! completion when the rebase stops on conflicts), run the optional
//! `[work.merge] verify_command` and push the rebased tip, wait for the
//! PR's checks to pass on the commit about to merge (GitHub issue #241, see
//! [`wait_for_checks`]: a failing check stops the merge, and checks that
//! outlast the timeout hand the ticket back as
//! [`MergeFlowOutcome::ChecksPending`]), merge the PR via `gh`
//! (rebasing and retrying once when GitHub rejects a stale base; GitHub
//! issue #109), fast-forward the local base branch, best-effort clean up
//! the worktree,
//! branch, the ticket's live runs (finished as `done`, their agents
//! stopped; GitHub issue #107), and ticket tmux session (its scrollback
//! archived first, see [`crate::work::session_gc`]; GitHub issue #78),
//! apply the configured
//! `status_on_merge` transition, clear any `tm:status/*` label GitHub left
//! on an issue the PR's closing keyword closed (GitHub issue #75), and
//! move the tickets of stacked PRs already merged into the branch, whose
//! closing keywords GitHub never honoured (GitHub issue #94), and finally
//! start a short headless agent session that archives the merged branch's
//! thatch memories (GitHub issue #208, see [`consolidate_branch_memories`];
//! a batch starts one session for every branch it merged).
//!
//! How the branch catches up with its base is `[work.merge].strategy`
//! (GitHub issue #244, see [`integrate_base`]): a rebase by default, a
//! squash into one commit and then a rebase, or a merge of the base into
//! the branch. Everywhere below, "rebase" also means that merge under the
//! `merge` strategy: the conflict prompt, the poll, and the hand-back hints
//! follow the strategy, and a merged branch is pushed without force.
//!
//! The conflict-resolution session is launched the same way an interactive
//! lane run is: [`crate::agent::AgentRunner::build_invocation`] under
//! [`crate::agent::RunMode::Interactive`], its prompt written to a file
//! under [`MergeDeps::state_dir`], and
//! [`crate::agent::AgentRunner::tmux_command_line`] rendering the command
//! that reads it back — the same billing-safety `env -u` prefix and
//! unattended default permissions (`bypassPermissions` for `ClaudeRunner`)
//! every other agent session gets. When a run store is configured, each
//! conflict session (one per single-ticket conflict, one shared row for a
//! batch's session) is recorded as a [`MERGE_CONFLICT_KIND`] run row,
//! pre-registered before the window opens and adopted by the session, then
//! finished by merge itself once the rebase resolves (`done`) or is handed
//! back (`failed`) — or by the session's SessionEnd hook if that comes
//! first (GitHub issue #156): see [`run_conflict_session`].
//!
//! A conflict session that never resolves is not a hard failure: the
//! rebase is deliberately left in progress on disk and
//! [`MergeFlowOutcome::ConflictsHandedBack`] is returned so the caller can
//! say so without treating it as an error.
//!
//! [`run_merge_batch`] is the multi-key form (`tm merge KEY1 KEY2 ...`): it
//! merges every cleanly-rebasing ticket in order, sets conflicted ones
//! aside, then resolves all of those in one shared agent session instead
//! of one window per ticket. Both forms share the same prepare/finish
//! stages, window launch, and per-tick rebase check.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use thiserror::Error;

use crate::agent::{AgentInvocation, AgentRunner, InvocationInputs, RunMode, shell_quote};
use crate::cli::pr::{PrCliError, resolve_watch_repo_root};
use crate::config::{BackendIdentity, LaneConfig, MergeConfig, MergeStrategy};
use crate::github::gh_cli::{ChecksState, CommitPull, GhCli, GhError, PrChecks, RepoMergeMethod};
use crate::github::pr::{find_issue_key, find_pr_for_ticket};
use crate::runs::pid::pid_alive;
use crate::runs::{FinishRun, RunStatus, RunStore, RunStoreError, StartRun};
use crate::ticketing::provider::TicketProvider;
use crate::ticketing::{StatusTransition, apply_status_on_merge, reconcile_status};
use crate::work::git::{GitError, GitOps, RebaseOutcome};
use crate::work::interactive::interactive_prompt;
use crate::work::naming::ticket_session_name;
use crate::work::prompt::{PromptFileError, read_prompt_file};
use crate::work::review_watch::{Clock, Sleeper};
use crate::work::session_gc;
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

/// Seconds between polls of a PR's checks while [`wait_for_checks`] waits
/// for them to finish (GitHub issue #241).
pub const CHECKS_POLL_SECS: u64 = 15;

/// Seconds between "still waiting" progress lines while the checks on a
/// PR's head are unchanged.
pub const CHECKS_HEARTBEAT_SECS: i64 = 120;

/// Base name (before [`unique_window_name`] suffixing) of the tmux window a
/// conflict-resolution session runs in.
pub const MERGE_WINDOW_BASE: &str = "merge";

/// The run kind, and lane, of a conflict-resolution session's run row
/// (GitHub issue #156).
pub const MERGE_CONFLICT_KIND: &str = "merge-conflict";

/// The run event a batch conflict session's row records, its detail
/// `{"tickets": [...]}` naming every ticket the shared session resolves
/// (the row's own `ticket` holds only the first).
pub const MERGE_BATCH_EVENT: &str = "merge-batch";

/// Base name (before [`unique_window_name`] suffixing) of the tmux window
/// the post-merge memory-consolidation session runs in (GitHub issue #208).
pub const MERGE_MEMORY_WINDOW_BASE: &str = "memory";

/// The run kind the memory-consolidation session is admitted under. No run
/// row is recorded for it: the session is headless with no supervisor to
/// finish a row, so the kind only keys admission's memory estimate.
pub const MERGE_MEMORY_KIND: &str = "merge-memory";

/// Turn budget for the memory-consolidation session: enough for one
/// `memory_list`, a `memory_show` per entry, and a remember plus forgets
/// per branch, and small because nothing tracks or reaps the session.
pub const MERGE_MEMORY_MAX_TURNS: &str = "30";

/// Errors returned by [`run_merge`].
#[derive(Debug, Error)]
pub enum MergeError {
    /// Memory or usage admission refused the conflict session (GitHub
    /// issue #149).
    #[error(transparent)]
    Admission(#[from] crate::work::admission::AdmissionError),

    /// A `git` shell-out failed.
    #[error(transparent)]
    Git(#[from] GitError),

    /// A `gh` shell-out failed.
    #[error(transparent)]
    Gh(#[from] GhError),

    /// A `tmux` shell-out failed.
    #[error(transparent)]
    Tmux(#[from] TmuxError),

    /// `state_dir` could not be created to hold the memory-consolidation
    /// session's log.
    #[error("failed to create {path}: {source}")]
    StateDir {
        /// The directory that could not be created.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

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

    /// The PR's base branch head commit has a failed check, so merging (or
    /// rebasing onto it) would build on a broken base (GitHub issue #242).
    /// `--allow-red-base` skips this gate.
    #[error(
        "{base} is red at {}: failing checks: {}. Fix {base} first, or rerun with --allow-red-base to merge anyway",
        short_sha(sha),
        failing.join(", ")
    )]
    BaseRed {
        /// The PR's base branch.
        base: String,
        /// The base branch's head commit.
        sha: String,
        /// The names of the failed checks on that commit, in GitHub's order.
        failing: Vec<String>,
    },

    /// A check failed on the PR's head commit, so the merge stopped before
    /// `gh pr merge` (GitHub issue #241). The PR stays open and the branch
    /// stays pushed.
    #[error(
        "checks failed on PR #{number}: {}; not merged (the branch stays pushed: fix it, then rerun `tm merge`)",
        checks.join(", ")
    )]
    ChecksFailed {
        /// The PR number.
        number: u64,
        /// The names of the failing checks.
        checks: Vec<String>,
    },

    /// `[work.merge] verify_command` exited non-zero after the rebase, so
    /// nothing was pushed or merged.
    #[error(
        "verify_command `{command}` failed in {}; nothing pushed (output in {})",
        dir.display(),
        log.display()
    )]
    VerifyFailed {
        /// The configured command.
        command: String,
        /// The checkout it ran in.
        dir: PathBuf,
        /// The file its output was written to.
        log: PathBuf,
    },

    /// `[work.merge] verify_command` could not be started.
    #[error("failed to run verify_command `{command}`: {source}")]
    VerifySpawn {
        /// The configured command.
        command: String,
        /// The underlying I/O error.
        source: std::io::Error,
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
    /// The PR's checks were still running when
    /// `[work.merge] checks_timeout_secs` ran out (GitHub issue #241). The
    /// branch is pushed and the PR left open; rerunning `tm merge` waits
    /// on the same head again.
    ChecksPending,
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
    /// candidate is considered. Also where a conflict session's
    /// `merge-conflict` row is recorded; `None` leaves the session
    /// untracked.
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
    /// Memory and usage admission for the conflict session's agent (GitHub
    /// issue #149).
    pub admission: crate::work::admission::AdmissionDeps<'a>,
    /// `[work] default_model` and the `[agent.<runner>]` models, which the
    /// conflict session's model resolution falls back to after `[work.merge]
    /// model` (GitHub issue #143).
    pub model_defaults: crate::work::model::ModelDefaults<'a>,
    /// Configured `status_on_merge` target status, if any.
    pub status_on_merge: Option<&'a str>,
    /// Directory the conflict session's prompt file is written under
    /// (`merge-<key>-<timestamp>.prompt.md`), the same run-state directory
    /// `tm work run`/`tm review fix` use for their own interactive prompt
    /// files. Created (via `create_dir_all`) if it doesn't exist yet.
    pub state_dir: &'a Path,
    /// The session marker directory
    /// ([`crate::runs::session::sessions_dir_from_process_env`] in
    /// production). When merge finishes a conflict session's row itself, it
    /// removes any marker pointing at that row, so the session's later
    /// SessionEnd hook is a no-op instead of overwriting the outcome.
    pub sessions_dir: &'a Path,
    /// Stops a merged ticket's still-running agent by pid (see
    /// [`finish_ticket_runs`]); production passes
    /// [`crate::runs::pid::kill_pid`], the same `SIGKILL` sender idle
    /// hibernation uses.
    pub kill_pid: &'a dyn Fn(u32),
    /// `tm merge --allow-red-base`: skip the red-base gate (see
    /// [`MergeError::BaseRed`]), for the operator merging the fix for the
    /// broken base itself.
    pub allow_red_base: bool,
    /// Runs `[work.merge] verify_command` (the second argument) in a
    /// checkout (the first), its output going to a log file under
    /// `state_dir` (the third), and reports whether it exited zero;
    /// production passes [`run_verify_command`].
    pub run_verify: &'a VerifyRunner,
}

/// Runs `[work.merge] verify_command` (the second argument) in a checkout
/// (the first), writing its output to a log file (the third), and reports
/// whether it exited zero. See [`MergeDeps::run_verify`].
pub type VerifyRunner = dyn Fn(&Path, &str, &Path) -> std::io::Result<bool>;

/// Production [`MergeDeps::run_verify`]: runs `command` with `sh -c` in
/// `dir` and reports whether it exited zero. Its stdout and stderr go to
/// `log` (truncated first) and its stdin is closed.
///
/// The output never shares tm's own stdout or stderr. The board runs
/// `tm merge` with both piped and reads them only after it exits, so a test
/// suite's worth of output would fill the pipe and block `tm merge`
/// forever. Keeping it out also leaves tm's own error as the first stderr
/// line the board shows.
pub fn run_verify_command(dir: &Path, command: &str, log: &Path) -> std::io::Result<bool> {
    let stdout = std::fs::File::create(log)?;
    let stderr = stdout.try_clone()?;
    std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .status()
        .map(|status| status.success())
}

/// The first 12 characters of a commit SHA, for messages.
pub fn short_sha(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

/// Refuses the merge when the head commit of the PR's base branch has a
/// failed check (GitHub issue #242). Pending checks do not block. A failure
/// to read the checks only warns: the gate protects against building on a
/// known-broken base, and an unreadable base is not known to be broken.
fn ensure_base_not_red(
    deps: &MergeDeps<'_>,
    repo_root: &Path,
    base: &str,
    out: &mut dyn Write,
) -> Result<(), MergeError> {
    if deps.allow_red_base {
        return Ok(());
    }
    match deps.gh.commit_checks(repo_root, base) {
        Ok(checks) => {
            let failing: Vec<String> = checks.failing().map(|c| c.name.clone()).collect();
            if failing.is_empty() {
                Ok(())
            } else {
                Err(MergeError::BaseRed {
                    base: base.to_string(),
                    sha: checks.sha,
                    failing,
                })
            }
        }
        Err(err) => {
            writeln!(
                out,
                "warning: could not read {base}'s checks ({err}); merging without the red-base check"
            )?;
            Ok(())
        }
    }
}

/// Substitutes `{key}`, `{branch}`, `{base}`, and `{upstream_context}` in
/// `template`, producing the prompt text handed to the conflict-resolution
/// agent session. `upstream_context` is the rendered list of upstream pull
/// requests ([`upstream_context`]); a template without the placeholder is
/// unaffected by it.
pub fn conflict_prompt(
    template: &str,
    key: &str,
    branch: &str,
    base: &str,
    upstream_context: &str,
) -> String {
    template
        .replace("{key}", key)
        .replace("{branch}", branch)
        .replace("{base}", base)
        .replace("{upstream_context}", upstream_context)
}

/// The byte budget the upstream pull request bodies in `{upstream_context}`
/// share, across every pull request (and, in a batch, every ticket). Bodies
/// are trimmed to fit it, the way [`crate::work::ci_fix::LOG_LIMIT_BYTES`]
/// caps failure logs. Titles, headers and trim markers come on top of it.
pub const UPSTREAM_CONTEXT_LIMIT_BYTES: usize = 12_000;

/// The `{upstream_context}` text when no upstream pull request touched the
/// conflicted files.
const NO_UPSTREAM_PRS: &str =
    "No pull requests merged into the base since this branch was cut changed the conflicted files.";

/// Renders `{upstream_context}` for one conflicted rebase in `dir`: the pull
/// requests behind the upstream commits that touched the conflicted files
/// (GitHub issue #246).
///
/// The commits are those in `<pre_rebase_rev>..origin/<base>` touching any of
/// `conflicted` ([`GitOps::commits_touching`]), so only work that landed on
/// the base after the branch was cut. Each maps to its pull requests through
/// [`GhCli::commit_pulls`]; a PR reached by several commits is listed once.
/// A lookup failure is not fatal: the text then says the context is
/// unavailable and why, and the session opens anyway.
fn upstream_context(
    deps: &MergeDeps<'_>,
    dir: &Path,
    pre_rebase_rev: &str,
    base: &str,
    conflicted: &[String],
    limit: usize,
) -> String {
    match upstream_pulls(deps, dir, pre_rebase_rev, base, conflicted) {
        Ok(pulls) => render_upstream_context(&pulls, limit),
        Err(err) => format!(
            "Upstream pull request context is unavailable ({err}); work out the intent of \
             the base's changes from the diff."
        ),
    }
}

/// The deduplicated pull requests behind [`upstream_context`]'s commits, in
/// commit order (newest first).
fn upstream_pulls(
    deps: &MergeDeps<'_>,
    dir: &Path,
    pre_rebase_rev: &str,
    base: &str,
    conflicted: &[String],
) -> Result<Vec<CommitPull>, String> {
    let shas = deps
        .git
        .commits_touching(dir, pre_rebase_rev, &format!("origin/{base}"), conflicted)
        .map_err(|err| err.to_string())?;
    let mut pulls: Vec<CommitPull> = Vec::new();
    for sha in shas {
        for pull in deps
            .gh
            .commit_pulls(dir, &sha)
            .map_err(|err| err.to_string())?
        {
            if !pulls.iter().any(|seen| seen.number == pull.number) {
                pulls.push(pull);
            }
        }
    }
    Ok(pulls)
}

/// Formats `pulls` as one `- PR #N: title` entry each, with the body
/// indented beneath it. Bodies share `limit` bytes evenly; a longer body
/// keeps its start and ends with a trim marker.
fn render_upstream_context(pulls: &[CommitPull], limit: usize) -> String {
    if pulls.is_empty() {
        return NO_UPSTREAM_PRS.to_string();
    }
    let per_body = limit / pulls.len();
    let entries: Vec<String> = pulls
        .iter()
        .map(|pull| {
            let mut entry = format!("- PR #{}: {}\n", pull.number, pull.title.trim());
            let body = trim_body(pull.body.trim(), per_body);
            for line in body.lines() {
                if !line.is_empty() {
                    entry.push_str("  ");
                }
                entry.push_str(line);
                entry.push('\n');
            }
            entry
        })
        .collect();
    format!(
        "Pull requests merged into the base that changed the conflicted files:\n\n{}",
        entries.join("\n")
    )
}

/// Keeps the first `limit` bytes of `body` (cut back to a char boundary),
/// marking how much was dropped.
fn trim_body(body: &str, limit: usize) -> String {
    if body.len() <= limit {
        return body.to_string();
    }
    let mut end = limit;
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[... {} bytes trimmed ...]",
        body[..end].trim_end(),
        body.len() - end
    )
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
/// behind. A template using `{upstream_context}` gets the upstream pull
/// requests that changed the conflicted files ([`upstream_context`]).
///
/// The session is launched the same way an interactive lane run is
/// ([`crate::work::interactive::launch_interactive_run`]'s command
/// construction, inlined here since there is no
/// [`crate::work::run::PreparedRun`] to hand that helper):
/// [`AgentRunner::build_invocation`] under [`RunMode::Interactive`] with
/// `permission_mode: None` (so it resolves to the runner's own default,
/// `bypassPermissions` for `ClaudeRunner` since issue #29 — never hardcoded
/// here), then [`AgentRunner::tmux_command_line`] renders the command that
/// reads the prompt back from disk. This is what makes the session run
/// unattended instead of stalling on the first permission prompt.
///
/// **Tracked as a `merge-conflict` row** (GitHub issue #156) when
/// [`MergeDeps::run_store`] is set, so the agent counts toward runs.db's
/// view of what is running: see [`start_conflict_run`]. The row is
/// pre-registered after the prompt is read (a bad `prompt_file` leaves no
/// row), its id rides into the session as `TSKMSTR_SESSION_RUN_ID` (never
/// `TSKMSTR_RUN_ID`, which would invert the SessionEnd hook), and the prompt
/// gains [`crate::work::interactive::interactive_prompt`]'s registration
/// preamble so the session adopts it. Merge finishes the row itself once
/// polling ends ([`finish_conflict_run`]): `done` when the rebase resolved,
/// `failed` when it was handed back or the session errored.
fn run_conflict_session(
    deps: &MergeDeps<'_>,
    prepared: &PreparedMerge,
    conflict: &PendingConflict,
    out: &mut dyn Write,
) -> Result<ConflictOutcome, MergeError> {
    let PreparedMerge {
        key, branch, base, ..
    } = prepared;
    let dir = conflict.dir.as_path();
    let pre_rebase_rev = conflict.pre_rebase_rev.as_str();
    let template = match deps.merge_cfg.prompt_file.as_deref() {
        Some(raw) => read_prompt_file(raw, deps.home, "[work.merge].prompt_file")?,
        None => match deps.merge_cfg.prompt.as_deref() {
            Some(prompt) => prompt.to_string(),
            None => match deps.merge_cfg.strategy {
                MergeStrategy::Merge => deps.runner.default_merge_commit_conflict_prompt_template(),
                MergeStrategy::Rebase | MergeStrategy::Squash => {
                    deps.runner.default_merge_conflict_prompt_template()
                }
            }
            .to_string(),
        },
    };
    let conflicted = deps.git.conflicted_files(dir)?;
    // Only a template that uses the placeholder pays for the git and gh
    // lookups.
    let context = if template.contains("{upstream_context}") {
        upstream_context(
            deps,
            dir,
            pre_rebase_rev,
            base,
            &conflicted,
            UPSTREAM_CONTEXT_LIMIT_BYTES,
        )
    } else {
        String::new()
    };
    let prompt = conflict_prompt(&template, key, branch, base, &context);
    let admitted = admit_conflict_session(deps, &prepared.repo_root, out)?;
    let run_id = start_conflict_run(deps, key, Some(branch), dir, &prepared.repo_root);
    let prompt = with_registration(run_id, key, prompt);

    let result = open_and_poll_conflict_session(
        deps,
        &admitted,
        prompt,
        prepared,
        dir,
        pre_rebase_rev,
        conflicted.len(),
        run_id,
        out,
    );
    let op = operation(deps.merge_cfg.strategy);
    let ending = match &result {
        Ok(ConflictOutcome::Resolved) => None,
        Ok(ConflictOutcome::HandedBack) => Some(format!(
            "{key}: {op} not completed (timed out, aborted, or the session ended); left in progress in {}",
            dir.display()
        )),
        Err(err) => Some(format!("{key}: conflict session failed: {err}")),
    };
    finish_conflict_run(deps, run_id, ending);
    if let Ok(ConflictOutcome::HandedBack) = &result {
        write_restore_hint(conflict, branch, out)?;
    }
    result
}

/// The word for what [`integrate_base`] runs under `strategy`, used in
/// progress and hand-back messages: `"merge"` for
/// [`MergeStrategy::Merge`], `"rebase"` otherwise (a squash is rebased).
fn operation(strategy: MergeStrategy) -> &'static str {
    match strategy {
        MergeStrategy::Merge => "merge",
        MergeStrategy::Rebase | MergeStrategy::Squash => "rebase",
    }
}

/// For a handed-back squash (see [`PendingConflict::restore_rev`]), names
/// the branch's original tip, since an abort leaves the branch squashed
/// and diverged from its remote, which a rerun of `tm merge` refuses.
fn write_restore_hint(
    conflict: &PendingConflict,
    branch: &str,
    out: &mut dyn Write,
) -> Result<(), MergeError> {
    if let Some(rev) = &conflict.restore_rev {
        writeln!(
            out,
            "{branch} was squashed before the rebase; to restore its original commits, abort any rebase in progress, then run: git -C {} reset --hard {rev}",
            conflict.dir.display()
        )?;
    }
    Ok(())
}

/// The window-open and poll half of [`run_conflict_session`], split out so
/// every way it can end (resolved, handed back, or an error) reaches that
/// function's row finish.
#[allow(clippy::too_many_arguments)]
fn open_and_poll_conflict_session(
    deps: &MergeDeps<'_>,
    admitted: &crate::work::admission::Admitted,
    prompt: String,
    prepared: &PreparedMerge,
    dir: &Path,
    pre_rebase_rev: &str,
    conflicted_count: usize,
    run_id: Option<i64>,
    out: &mut dyn Write,
) -> Result<ConflictOutcome, MergeError> {
    let key = &prepared.key;
    let session = open_agent_window(deps, admitted, prompt, key, key, dir, run_id)?;
    let AgentWindow {
        target,
        window,
        inside_tmux,
    } = &session;

    writeln!(
        out,
        "conflicts in {conflicted_count} files; opening agent session (window {target}:{window})"
    )?;
    if !inside_tmux {
        writeln!(out, "attach: tmux attach -t {target}")?;
    }

    poll_conflict_session(
        deps,
        dir,
        target,
        window,
        &prepared.branch,
        pre_rebase_rev,
        out,
    )
}

/// Admits a conflict session's agent (GitHub issue #149), before its row is
/// pre-registered so a refusal leaves none. `tm merge` is human-started:
/// over the memory budget it refuses, over a usage ceiling it warns and
/// launches. With no [`MergeDeps::run_store`] there is nothing to count, so
/// the session launches with a warning.
fn admit_conflict_session(
    deps: &MergeDeps<'_>,
    repo_root: &Path,
    out: &mut dyn Write,
) -> Result<crate::work::admission::Admitted, MergeError> {
    let repo = crate::work::run::dir_repo_key(repo_root);
    Ok(deps.admission.admit_tracked(
        deps.run_store,
        &crate::work::admission::LaunchRequest {
            kind: MERGE_CONFLICT_KIND,
            agent: deps.runner.name(),
            repo: &repo,
            own_run: None,
        },
        crate::work::admission::LaunchOrigin::Human,
        out,
    )?)
}

/// Pre-registers a conflict session's `merge-conflict` row (GitHub issue
/// #156) and returns its id, or `None` when [`MergeDeps::run_store`] is
/// unset or the store refused.
///
/// `lane` and `kind` are both [`MERGE_CONFLICT_KIND`], `ticket` the (first)
/// conflicted key, `worktree` the checkout the rebase is in progress in,
/// `pid` `None` until the window opens (see [`open_agent_window`]). `agent`
/// is the runner's name and `repo` the main checkout's directory name
/// ([`crate::work::run::dir_repo_key`] of `repo_root`), not the worktree's,
/// so a conflict in any of a repo's worktrees estimates under the same key
/// as that repo's lanes.
///
/// Store errors are swallowed here and in [`finish_conflict_run`]: the row
/// is telemetry, and a broken runs DB must never fail a merge.
fn start_conflict_run(
    deps: &MergeDeps<'_>,
    ticket: &str,
    branch: Option<&str>,
    dir: &Path,
    repo_root: &Path,
) -> Option<i64> {
    let store = deps.run_store?;
    let id = store
        .start_run(&StartRun {
            scope: deps.identity.scope(),
            ticket: ticket.to_string(),
            lane: MERGE_CONFLICT_KIND.to_string(),
            worktree: dir.to_string_lossy().into_owned(),
            branch: branch.map(str::to_string),
            pid: None,
            kind: MERGE_CONFLICT_KIND.to_string(),
            log_path: None,
        })
        .ok()?;
    let _ = store.update_agent_repo(
        id,
        deps.runner.name(),
        &crate::work::run::dir_repo_key(repo_root),
    );
    Some(id)
}

/// `prompt` with the registration preamble prepended when the session has a
/// row to adopt; unchanged for an untracked session.
fn with_registration(run_id: Option<i64>, ticket: &str, prompt: String) -> String {
    match run_id {
        Some(_) => interactive_prompt(MERGE_CONFLICT_KIND, ticket, &prompt),
        None => prompt,
    }
}

/// Finishes a conflict session's row once merge stops polling: `done` when
/// `failure` is `None`, otherwise `failed` with `failure` as its blocker.
/// A no-op for an untracked session.
///
/// Uses [`RunStore::finish_run_from_supervisor`] without forcing, so a row
/// the session's own SessionEnd hook already finished keeps that outcome,
/// then removes every session marker naming the row
/// ([`crate::runs::session::remove_markers_for_run`]) so a hook that fires
/// later finds nothing to finish. Whichever of the two ends the row first
/// wins; the other is a no-op. Errors are swallowed, as in
/// [`start_conflict_run`].
fn finish_conflict_run(deps: &MergeDeps<'_>, run_id: Option<i64>, failure: Option<String>) {
    let (Some(store), Some(id)) = (deps.run_store, run_id) else {
        return;
    };
    let outcome = FinishRun {
        status: if failure.is_some() {
            RunStatus::Failed
        } else {
            RunStatus::Done
        },
        blocker: failure,
        ..FinishRun::default()
    };
    let _ = store.finish_run_from_supervisor(id, &outcome, false);
    let _ = crate::runs::session::remove_markers_for_run(deps.sessions_dir, id);
}

/// Where [`open_agent_window`] opened a conflict-resolution session.
pub(crate) struct AgentWindow {
    /// The tmux session the window lives in.
    pub(crate) target: String,
    /// The window's name within `target`.
    pub(crate) window: String,
    /// Whether `target` is the invoking process's own tmux session (so no
    /// attach hint is needed).
    pub(crate) inside_tmux: bool,
}

/// Writes `prompt` to `merge-<prompt_stem>-<timestamp>.prompt.md` under
/// [`MergeDeps::state_dir`] and opens a tmux window in `dir` running the
/// agent against it — on the current tmux session when there is one,
/// otherwise on `session_key`'s ticket session (created if needed).
///
/// The prompt file is written before any tmux call, so a `state_dir` write
/// failure never leaves a half-opened session behind. See
/// [`run_conflict_session`] for why the session is launched this way.
///
/// `run_id` is the session's pre-registered row, if any. It goes into the
/// invocation as an interactive run id, so the window's environment carries
/// `TSKMSTR_SESSION_RUN_ID` (the invocation's `env_set` is passed to tmux,
/// the same as [`crate::work::interactive::launch_interactive_run`]). Once
/// the window exists the row is stamped, best-effort, with the pane's pid,
/// which keeps an agent that has not adopted the row yet from being reaped
/// stale partway through [`CONFLICT_TIMEOUT_SECS`]. The row's tmux session
/// is stamped only when tm created the ticket session for it: inside tmux
/// the window joins the user's own session, which outlives the agent and so
/// says nothing about whether the row is still live.
fn open_agent_window(
    deps: &MergeDeps<'_>,
    admitted: &crate::work::admission::Admitted,
    prompt: String,
    prompt_stem: &str,
    session_key: &str,
    dir: &Path,
    run_id: Option<i64>,
) -> Result<AgentWindow, MergeError> {
    let model = deps
        .model_defaults
        .resolve(
            deps.runner,
            &[crate::work::model::ModelCandidate {
                source: "[work.merge] model",
                model: deps.merge_cfg.model.as_deref(),
            }],
        )
        .model;
    let invocation = deps.runner.build_invocation(
        admitted,
        InvocationInputs {
            prompt,
            model,
            max_turns: None,
            permission_mode: None,
            settings_path: None,
            run_id: run_id.map(|id| id.to_string()),
            mode: RunMode::Interactive,
            tool_scope: crate::agent::ToolScope::Full,
        },
    );
    let prompt_text = deps
        .runner
        .interactive_prompt(&invocation)
        .unwrap_or_default();
    let prompt_path = deps.state_dir.join(format!(
        "merge-{prompt_stem}-{}.prompt.md",
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
    let opened = open_command_window(
        deps.tmux,
        deps.identity,
        session_key,
        MERGE_WINDOW_BASE,
        dir,
        &invocation.env_set,
        &command,
    )?;

    if let (Some(store), Some(id)) = (deps.run_store, run_id) {
        if !opened.inside_tmux {
            let _ = store.update_tmux_session(id, &opened.target);
        }
        if let Ok(Some(pid)) = deps.tmux.pane_pid(&opened.target, &opened.window) {
            let _ = store.update_pid(id, pid);
        }
    }

    Ok(opened)
}

/// Opens a tmux window named after `window_base` in `dir` running
/// `command`: on the current tmux session when there is one, otherwise on
/// `session_key`'s ticket session, created if needed. Shared by the
/// conflict sessions ([`open_agent_window`]), the memory-consolidation
/// session ([`start_memory_session`]), and the defect retro session
/// ([`crate::work::retro::start_defect_session`]).
pub(crate) fn open_command_window(
    tmux: &dyn TmuxOps,
    identity: &BackendIdentity,
    session_key: &str,
    window_base: &str,
    dir: &Path,
    env: &[(String, String)],
    command: &str,
) -> Result<AgentWindow, TmuxError> {
    let current_session = tmux.current_session_name()?;
    let inside_tmux = current_session.is_some();
    let target = current_session
        .unwrap_or_else(|| ticket_session_name(&identity.session_slug(), session_key));

    let windows = tmux.list_windows()?;
    let existing = session_window_names(&windows, &target);
    let window = unique_window_name(window_base, &existing);
    let dir_str = dir.to_string_lossy().into_owned();

    if existing.is_empty() {
        tmux.new_session_with_command(&target, &dir_str, &window, env, command)?;
    } else {
        tmux.new_window_with_command(&target, &window, &dir_str, env, command)?;
    }

    Ok(AgentWindow {
        target,
        window,
        inside_tmux,
    })
}

/// Starts the post-merge memory-consolidation session for `branches`, all
/// merged from `repo_root` (GitHub issue #208), when
/// [`MergeConfig::consolidate_memories`] is on. Advisory: a failure is
/// reported as a warning on `out` and never fails the merge or changes its
/// exit code.
fn consolidate_branch_memories(
    deps: &MergeDeps<'_>,
    repo_root: &Path,
    session_key: &str,
    branches: &[&str],
    out: &mut dyn Write,
) {
    if !deps.merge_cfg.consolidate_memories || branches.is_empty() {
        return;
    }
    if let Err(err) = start_memory_session(deps, repo_root, session_key, branches, out) {
        let _ = writeln!(out, "warning: memory consolidation not started: {err}");
    }
}

/// The launch half of [`consolidate_branch_memories`]: admits the agent,
/// builds a [`RunMode::Headless`] invocation of the runner's
/// [`AgentRunner::default_memory_consolidation_prompt_template`], and opens
/// it in a [`MERGE_MEMORY_WINDOW_BASE`] window in `repo_root` (the merged
/// worktrees are gone by now, and thatch finds the project store from the
/// repo's remote) via [`open_command_window`].
///
/// Headless rather than interactive so the agent exits, and its window
/// closes, once it is done; its output goes to
/// `merge-memory-<session_key>-<timestamp>.log` under
/// [`MergeDeps::state_dir`]. The session is untracked: no run row, no run-id
/// variable, and no telemetry settings.
///
/// Because it runs untracked in `repo_root`, the operator's own checkout,
/// with no worktree to contain it, it does not take the lane default of a
/// bypassed permission mode and a 200-turn budget. It runs with
/// [`ToolScope::ThatchOnly`](crate::agent::ToolScope::ThatchOnly), so it can
/// call thatch's MCP tools and nothing else, and with
/// [`MERGE_MEMORY_MAX_TURNS`] turns.
fn start_memory_session(
    deps: &MergeDeps<'_>,
    repo_root: &Path,
    session_key: &str,
    branches: &[&str],
    out: &mut dyn Write,
) -> Result<(), MergeError> {
    let branch_list = branches
        .iter()
        .map(|branch| format!("- {branch}"))
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = deps
        .runner
        .default_memory_consolidation_prompt_template()
        .replace("{branch_list}", &branch_list);

    let repo = crate::work::run::dir_repo_key(repo_root);
    let admitted = deps.admission.admit_tracked(
        deps.run_store,
        &crate::work::admission::LaunchRequest {
            kind: MERGE_MEMORY_KIND,
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
                source: "[work.merge] model",
                model: deps.merge_cfg.model.as_deref(),
            }],
        )
        .model;
    let invocation = deps.runner.build_invocation(
        &admitted,
        InvocationInputs {
            prompt,
            model,
            max_turns: Some(MERGE_MEMORY_MAX_TURNS.to_string()),
            permission_mode: None,
            settings_path: None,
            run_id: None,
            mode: RunMode::Headless,
            tool_scope: crate::agent::ToolScope::ThatchOnly,
        },
    );

    std::fs::create_dir_all(deps.state_dir).map_err(|source| MergeError::StateDir {
        path: deps.state_dir.to_path_buf(),
        source,
    })?;
    let log_path = deps.state_dir.join(format!(
        "merge-memory-{session_key}-{}.log",
        deps.clock.now_unix_secs()
    ));
    let command = headless_command_line(&invocation, &log_path);
    let opened = open_command_window(
        deps.tmux,
        deps.identity,
        session_key,
        MERGE_MEMORY_WINDOW_BASE,
        repo_root,
        &invocation.env_set,
        &command,
    )?;

    writeln!(
        out,
        "consolidating thatch memories for {} (window {}:{}, log {})",
        branches.join(", "),
        opened.target,
        opened.window,
        log_path.display()
    )?;
    Ok(())
}

/// Renders a headless `invocation` as the command a tmux window runs:
/// `exec env -u VAR ... <program> <args...> > <log_path> 2>&1`, every
/// argument [`shell_quote`]d. The `env -u` prefix keeps
/// [`AgentInvocation::env_remove`]'s billing-safety stripping, which tmux
/// cannot express itself (see [`AgentRunner::tmux_command_line`]), and
/// `exec` lets the window close as soon as the agent exits.
pub(crate) fn headless_command_line(invocation: &AgentInvocation, log_path: &Path) -> String {
    let mut parts = vec!["exec".to_string(), "env".to_string()];
    for var in &invocation.env_remove {
        parts.push("-u".to_string());
        parts.push(var.clone());
    }
    parts.push(invocation.program.clone());
    parts.extend(invocation.args.iter().map(|arg| shell_quote(arg)));
    parts.push(">".to_string());
    parts.push(shell_quote(&log_path.to_string_lossy()));
    parts.push("2>&1".to_string());
    parts.join(" ")
}

/// One poll tick's view of a conflicted rebase, from [`check_rebase`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RebaseCheck {
    /// The rebase is still in progress.
    InProgress,
    /// The rebase completed: clean tree, back on the branch, tip rewritten.
    Resolved,
    /// The rebase state cleared but the tip is unchanged — `git rebase
    /// --abort` restored it.
    Aborted,
    /// The rebase state cleared but the tree is dirty or off the branch.
    Incomplete,
}

/// Inspects `dir`'s rebase state once (its merge state under
/// [`MergeStrategy::Merge`]: `git merge --abort` likewise restores the
/// pre-merge tip, and committing the merge always moves it). Shared by the
/// single-ticket poll loop ([`poll_conflict_session`]) and the batch one
/// ([`poll_batch_conflict_session`]).
///
/// `pre_rebase_rev` is the branch tip captured before `rebase_onto` ran:
/// `git rebase --abort` restores it exactly, while a completed rebase
/// always rewrites it (a rebase only runs when the base moved), so it is
/// what distinguishes "resolved" from "aborted" once the rebase state
/// clears.
fn check_rebase(
    deps: &MergeDeps<'_>,
    dir: &Path,
    branch: &str,
    pre_rebase_rev: &str,
) -> Result<RebaseCheck, MergeError> {
    let in_progress = match deps.merge_cfg.strategy {
        MergeStrategy::Merge => deps.git.merge_in_progress(dir)?,
        MergeStrategy::Rebase | MergeStrategy::Squash => deps.git.rebase_in_progress(dir)?,
    };
    if in_progress {
        return Ok(RebaseCheck::InProgress);
    }
    let clean = deps.git.status_is_clean(dir)?;
    let on_branch = deps.git.current_branch(dir).ok().as_deref() == Some(branch);
    if !(clean && on_branch) {
        return Ok(RebaseCheck::Incomplete);
    }
    Ok(match deps.git.rev_parse(dir, branch) {
        Ok(tip) if tip != pre_rebase_rev => RebaseCheck::Resolved,
        _ => RebaseCheck::Aborted,
    })
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
    let op = operation(deps.merge_cfg.strategy);
    let start = deps.clock.now_unix_secs();
    let mut last_count: Option<usize> = None;
    let mut last_heartbeat = start;

    loop {
        // Resolution is checked before the timeout so a rebase that
        // finished right at the deadline still counts as resolved.
        match check_rebase(deps, dir, branch, pre_rebase_rev)? {
            RebaseCheck::InProgress => {}
            RebaseCheck::Resolved => {
                writeln!(out, "conflicts resolved; {op} complete")?;
                return Ok(ConflictOutcome::Resolved);
            }
            RebaseCheck::Aborted => {
                writeln!(out, "{op} was aborted; handing back")?;
                return Ok(ConflictOutcome::HandedBack);
            }
            RebaseCheck::Incomplete => {
                writeln!(out, "conflict session ended without completing the {op}")?;
                return Ok(ConflictOutcome::HandedBack);
            }
        }

        let now = deps.clock.now_unix_secs();
        if now - start > CONFLICT_TIMEOUT_SECS {
            writeln!(
                out,
                "conflict resolution timed out after {CONFLICT_TIMEOUT_SECS}s"
            )?;
            writeln!(out, "attach: tmux attach -t {target}")?;
            writeln!(out, "abort: git -C {} {op} --abort", dir.display())?;
            return Ok(ConflictOutcome::HandedBack);
        }

        let windows = deps.tmux.list_windows()?;
        if !has_live_window(&windows, target, window) {
            writeln!(out, "conflict session ended without completing the {op}")?;
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
    let mut prepared = prepare_merge(deps, key, out)?;
    if let Some(conflict) = prepared.conflict.take() {
        match run_conflict_session(deps, &prepared, &conflict, out)? {
            ConflictOutcome::Resolved => prepared.needs_push = true,
            ConflictOutcome::HandedBack => return Ok(MergeFlowOutcome::ConflictsHandedBack),
        }
    }
    let outcome = finish_merge(deps, &prepared, None, out)?;
    if outcome == MergeFlowOutcome::Merged {
        consolidate_branch_memories(
            deps,
            &prepared.repo_root,
            &prepared.key,
            &[prepared.branch.as_str()],
            out,
        );
    }
    Ok(outcome)
}

/// What [`run_merge_batch`] accomplished, ticket by ticket.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BatchMergeReport {
    /// Keys whose PR was merged, in the order they merged.
    pub merged: Vec<String>,
    /// Keys whose conflict resolution never finished (timed out, aborted,
    /// or the session window died). Each rebase is left in progress (or
    /// aborted) on disk, exactly like a single-ticket
    /// [`MergeFlowOutcome::ConflictsHandedBack`].
    pub handed_back: Vec<String>,
    /// Keys that failed with an error, each paired with the error message.
    /// One ticket's failure never stops the rest of the batch.
    pub failed: Vec<(String, String)>,
    /// Keys whose PR checks were still running at the timeout (see
    /// [`MergeFlowOutcome::ChecksPending`]); pushed but not merged.
    pub checks_pending: Vec<String>,
}

impl BatchMergeReport {
    /// The `tm merge` exit code for this batch: `1` if any ticket failed,
    /// otherwise `2` if any was handed back or still has checks pending,
    /// otherwise `0`. A failure outranks a hand-back because it needs
    /// investigating, not just finishing.
    pub fn exit_code(&self) -> u8 {
        if !self.failed.is_empty() {
            1
        } else if !self.handed_back.is_empty() || !self.checks_pending.is_empty() {
            2
        } else {
            0
        }
    }
}

/// `tm merge KEY1 KEY2 ...`: merge several tickets' PRs, sharing one
/// conflict-resolution session across every ticket whose rebase stops on
/// conflicts.
///
/// **Phase 1** walks `keys` in order, strictly sequentially (parallel
/// rebases would race on the repo's index and worktrees). Each ticket runs
/// the same pre-rebase steps as [`run_merge`] and attempts its rebase. A
/// ticket that rebases cleanly (or needs no rebase) is merged, synced,
/// cleaned up, and transitioned immediately. A ticket that stops on
/// conflicts is set aside with its rebase left in progress on disk. A
/// ticket that errors is recorded in [`BatchMergeReport::failed`] and the
/// batch moves on.
///
/// **Phase 2** runs only if tickets were set aside. Exactly one set aside
/// gets the ordinary single-ticket session (honoring
/// `[work.merge].prompt`/`prompt_file`). Two or more share one agent window
/// whose prompt comes from `[work.merge].batch_conflict_prompt` (or the
/// runner's [`AgentRunner::default_batch_merge_conflict_prompt_template`])
/// with `{ticket_list}` substituted. Every set-aside rebase is polled
/// together until each resolves, is abandoned, the window dies, or
/// [`CONFLICT_TIMEOUT_SECS`] per set-aside ticket elapses. Tickets that
/// resolved are then pushed and merged in their original order; the rest
/// are handed back. A ticket an earlier merge left behind its base is
/// rebased and retried once (see [`merge_pr_with_retry`]); one whose retry
/// rebase conflicts and never resolves is handed back too.
///
/// Returns `Err` only when writing to `out` fails between tickets;
/// per-ticket failures land in the report instead.
pub fn run_merge_batch(
    deps: &MergeDeps<'_>,
    keys: &[String],
    out: &mut dyn Write,
) -> Result<BatchMergeReport, MergeError> {
    let mut report = BatchMergeReport::default();
    let mut set_aside: Vec<(PreparedMerge, PendingConflict)> = Vec::new();
    let mut merged: Vec<PreparedMerge> = Vec::new();
    let total = keys.len();

    for (i, key) in keys.iter().enumerate() {
        writeln!(out, "=== [{}/{total}] {key} ===", i + 1)?;
        let mut prepared = match prepare_merge(deps, key, out) {
            Ok(prepared) => prepared,
            Err(err) => {
                record_failure(&mut report, key, &err, out)?;
                continue;
            }
        };
        match prepared.conflict.take() {
            Some(conflict) => {
                writeln!(
                    out,
                    "{key}: {} stopped on conflicts; set aside for the conflict session",
                    operation(deps.merge_cfg.strategy)
                )?;
                set_aside.push((prepared, conflict));
            }
            None => {
                if finish_into_report(deps, &prepared, None, &mut report, out)? {
                    merged.push(prepared);
                }
            }
        }
    }

    // The shared conflict session, kept alive while any ticket is handed
    // back to it (see [`finish_merge`]'s `kept_session`).
    let mut kept_session: Option<String> = None;
    let resolved: Vec<PreparedMerge> = match set_aside.len() {
        0 => Vec::new(),
        1 => {
            let (prepared, conflict) = set_aside.remove(0);
            writeln!(out, "=== conflict session: {} ===", prepared.key)?;
            match run_conflict_session(deps, &prepared, &conflict, out) {
                Ok(ConflictOutcome::Resolved) => vec![prepared],
                Ok(ConflictOutcome::HandedBack) => {
                    report.handed_back.push(prepared.key);
                    Vec::new()
                }
                Err(err) => {
                    record_failure(&mut report, &prepared.key, &err, out)?;
                    Vec::new()
                }
            }
        }
        n => {
            writeln!(out, "=== shared conflict session: {n} tickets ===")?;
            match run_batch_conflict_session(deps, &set_aside, out) {
                Ok((flags, target)) => {
                    if flags.contains(&false) {
                        kept_session = Some(target);
                    }
                    let mut resolved = Vec::new();
                    for ((prepared, conflict), ok) in set_aside.into_iter().zip(flags) {
                        if ok {
                            resolved.push(prepared);
                        } else {
                            write_restore_hint(&conflict, &prepared.branch, out)?;
                            report.handed_back.push(prepared.key);
                        }
                    }
                    resolved
                }
                Err(err) => {
                    for (prepared, _) in &set_aside {
                        record_failure(&mut report, &prepared.key, &err, out)?;
                    }
                    Vec::new()
                }
            }
        }
    };

    for mut prepared in resolved {
        writeln!(out, "=== finishing {} ===", prepared.key)?;
        prepared.needs_push = true;
        if finish_into_report(deps, &prepared, kept_session.as_deref(), &mut report, out)? {
            merged.push(prepared);
        }
    }

    consolidate_batch_memories(deps, &merged, out);

    writeln!(
        out,
        "batch done: {} merged, {} handed back, {} checks pending, {} failed",
        report.merged.len(),
        report.handed_back.len(),
        report.checks_pending.len(),
        report.failed.len()
    )?;
    for key in &report.handed_back {
        writeln!(
            out,
            "  handed back: {key} (finish the {}, then rerun `tm merge {key}`)",
            operation(deps.merge_cfg.strategy)
        )?;
    }
    for key in &report.checks_pending {
        writeln!(
            out,
            "  checks pending: {key} (rerun `tm merge {key}` once its checks finish)"
        )?;
    }
    for (key, err) in &report.failed {
        writeln!(out, "  failed: {key}: {err}")?;
    }
    Ok(report)
}

/// Records `key`'s failure in `report` and says so on `out`.
fn record_failure(
    report: &mut BatchMergeReport,
    key: &str,
    err: &MergeError,
    out: &mut dyn Write,
) -> Result<(), MergeError> {
    writeln!(out, "error: {key}: {err}")?;
    report.failed.push((key.to_string(), err.to_string()));
    Ok(())
}

/// Runs [`finish_merge`] for `prepared`, recording the result in `report`,
/// and returns whether the PR merged. `kept_session` is passed through (see
/// [`finish_merge`]).
fn finish_into_report(
    deps: &MergeDeps<'_>,
    prepared: &PreparedMerge,
    kept_session: Option<&str>,
    report: &mut BatchMergeReport,
    out: &mut dyn Write,
) -> Result<bool, MergeError> {
    match finish_merge(deps, prepared, kept_session, out) {
        Ok(MergeFlowOutcome::Merged) => {
            report.merged.push(prepared.key.clone());
            Ok(true)
        }
        Ok(MergeFlowOutcome::ConflictsHandedBack) => {
            report.handed_back.push(prepared.key.clone());
            Ok(false)
        }
        Ok(MergeFlowOutcome::ChecksPending) => {
            report.checks_pending.push(prepared.key.clone());
            Ok(false)
        }
        Err(err) => record_failure(report, &prepared.key, &err, out).map(|()| false),
    }
}

/// Starts one memory-consolidation session (see
/// [`consolidate_branch_memories`]) naming every branch a batch merged.
/// Branches are grouped by repo root, in merge order, since thatch's project
/// store is per repo: a batch within one repo, the usual case, gets exactly
/// one session, keyed on its first merged ticket.
fn consolidate_batch_memories(deps: &MergeDeps<'_>, merged: &[PreparedMerge], out: &mut dyn Write) {
    let mut groups: Vec<(&Path, &str, Vec<&str>)> = Vec::new();
    for prepared in merged {
        let root = prepared.repo_root.as_path();
        match groups
            .iter_mut()
            .find(|(group_root, ..)| *group_root == root)
        {
            Some((_, _, branches)) => branches.push(&prepared.branch),
            None => groups.push((root, &prepared.key, vec![&prepared.branch])),
        }
    }
    for (root, key, branches) in groups {
        consolidate_branch_memories(deps, root, key, &branches, out);
    }
}

/// Renders the `{ticket_list}` placeholder: one line per ticket giving its
/// key, branch, base, and the checkout directory its rebase (or, under
/// [`MergeStrategy::Merge`], its merge) is in progress in.
fn batch_ticket_list(
    strategy: MergeStrategy,
    tickets: &[(PreparedMerge, PendingConflict)],
) -> String {
    tickets
        .iter()
        .map(|(prepared, conflict)| match strategy {
            MergeStrategy::Merge => format!(
                "- {}: origin/{} into branch {} in {}",
                prepared.key,
                prepared.base,
                prepared.branch,
                conflict.dir.display()
            ),
            MergeStrategy::Rebase | MergeStrategy::Squash => format!(
                "- {}: branch {} onto origin/{} in {}",
                prepared.key,
                prepared.branch,
                prepared.base,
                conflict.dir.display()
            ),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Renders the batch prompt's `{upstream_context}` placeholder: one
/// `<KEY> upstream context:` section per ticket, each built like a single
/// ticket's [`upstream_context`], sharing [`UPSTREAM_CONTEXT_LIMIT_BYTES`].
fn batch_upstream_context(
    deps: &MergeDeps<'_>,
    tickets: &[(PreparedMerge, PendingConflict)],
) -> Result<String, MergeError> {
    let limit = UPSTREAM_CONTEXT_LIMIT_BYTES / tickets.len().max(1);
    let mut sections = Vec::new();
    for (prepared, conflict) in tickets {
        let conflicted = deps.git.conflicted_files(&conflict.dir)?;
        let context = upstream_context(
            deps,
            &conflict.dir,
            &conflict.pre_rebase_rev,
            &prepared.base,
            &conflicted,
            limit,
        );
        sections.push(format!("{} upstream context:\n{context}", prepared.key));
    }
    Ok(sections.join("\n\n"))
}

/// Opens the one agent window shared by every set-aside ticket, then polls
/// them all. Returns one flag per ticket, in order (`true` if its rebase
/// resolved), plus the tmux session the shared window was opened in.
///
/// Tracked like [`run_conflict_session`], with one `merge-conflict` row for
/// the whole shared session (GitHub issue #156): its `ticket` and the
/// registration preamble's key are the first set-aside ticket's, its
/// `worktree` that ticket's checkout (where the window opens), and its
/// `branch` `None`, since the session works on several branches. Every
/// set-aside key is recorded in a [`MERGE_BATCH_EVENT`] event. The row ends
/// `done` only if every rebase resolved, `failed` (naming the handed-back
/// keys) otherwise.
fn run_batch_conflict_session(
    deps: &MergeDeps<'_>,
    tickets: &[(PreparedMerge, PendingConflict)],
    out: &mut dyn Write,
) -> Result<(Vec<bool>, String), MergeError> {
    let strategy = deps.merge_cfg.strategy;
    let template = match deps.merge_cfg.batch_conflict_prompt.as_deref() {
        Some(prompt) => prompt.to_string(),
        None => match strategy {
            MergeStrategy::Merge => deps
                .runner
                .default_batch_merge_commit_conflict_prompt_template(),
            MergeStrategy::Rebase | MergeStrategy::Squash => {
                deps.runner.default_batch_merge_conflict_prompt_template()
            }
        }
        .to_string(),
    };
    let prompt = template.replace("{ticket_list}", &batch_ticket_list(strategy, tickets));
    let prompt = if prompt.contains("{upstream_context}") {
        prompt.replace(
            "{upstream_context}",
            &batch_upstream_context(deps, tickets)?,
        )
    } else {
        prompt
    };

    // The window opens in the first ticket's checkout; the prompt names
    // every other ticket's directory explicitly.
    let (first, first_conflict) = &tickets[0];
    let keys: Vec<&str> = tickets.iter().map(|(p, _)| p.key.as_str()).collect();
    let admitted = admit_conflict_session(deps, &first.repo_root, out)?;
    let run_id = start_conflict_run(
        deps,
        &first.key,
        None,
        &first_conflict.dir,
        &first.repo_root,
    );
    if let (Some(store), Some(id)) = (deps.run_store, run_id) {
        let detail = serde_json::json!({ "tickets": keys }).to_string();
        let _ = store.add_event(id, MERGE_BATCH_EVENT, Some(&detail));
    }
    let prompt = with_registration(run_id, &first.key, prompt);

    let result =
        open_and_poll_batch_conflict_session(deps, &admitted, prompt, tickets, run_id, out);
    let ending = match &result {
        Ok((flags, _)) => {
            let handed_back: Vec<&str> = keys
                .iter()
                .zip(flags)
                .filter(|(_, resolved)| !**resolved)
                .map(|(key, _)| *key)
                .collect();
            (!handed_back.is_empty()).then(|| {
                format!(
                    "{} not completed for {}; left in progress",
                    operation(strategy),
                    handed_back.join(", ")
                )
            })
        }
        Err(err) => Some(format!("shared conflict session failed: {err}")),
    };
    finish_conflict_run(deps, run_id, ending);
    result
}

/// The window-open and poll half of [`run_batch_conflict_session`], split
/// out for the same reason as [`open_and_poll_conflict_session`].
fn open_and_poll_batch_conflict_session(
    deps: &MergeDeps<'_>,
    admitted: &crate::work::admission::Admitted,
    prompt: String,
    tickets: &[(PreparedMerge, PendingConflict)],
    run_id: Option<i64>,
    out: &mut dyn Write,
) -> Result<(Vec<bool>, String), MergeError> {
    let (first, first_conflict) = &tickets[0];
    let session = open_agent_window(
        deps,
        admitted,
        prompt,
        "batch",
        &first.key,
        &first_conflict.dir,
        run_id,
    )?;
    let AgentWindow {
        target,
        window,
        inside_tmux,
    } = &session;

    let keys: Vec<&str> = tickets.iter().map(|(p, _)| p.key.as_str()).collect();
    writeln!(
        out,
        "conflicts in {} tickets ({}); opening agent session (window {target}:{window})",
        tickets.len(),
        keys.join(", ")
    )?;
    if !inside_tmux {
        writeln!(out, "attach: tmux attach -t {target}")?;
    }

    let flags = poll_batch_conflict_session(deps, tickets, target, window, out)?;
    Ok((flags, session.target))
}

/// The batch counterpart of [`poll_conflict_session`]: each tick runs
/// [`check_rebase`] on every still-pending ticket, then applies the shared
/// timeout ([`CONFLICT_TIMEOUT_SECS`] per ticket, since the agent resolves
/// them one after another) and the shared window's liveness check to
/// whatever is still pending.
fn poll_batch_conflict_session(
    deps: &MergeDeps<'_>,
    tickets: &[(PreparedMerge, PendingConflict)],
    target: &str,
    window: &str,
    out: &mut dyn Write,
) -> Result<Vec<bool>, MergeError> {
    let op = operation(deps.merge_cfg.strategy);
    let timeout = CONFLICT_TIMEOUT_SECS * tickets.len() as i64;
    let start = deps.clock.now_unix_secs();
    let mut resolved = vec![false; tickets.len()];
    let mut pending: Vec<usize> = (0..tickets.len()).collect();
    let mut last_count: Option<usize> = None;
    let mut last_heartbeat = start;

    loop {
        let mut still_pending = Vec::new();
        for &i in &pending {
            let (prepared, conflict) = &tickets[i];
            let key = &prepared.key;
            match check_rebase(
                deps,
                &conflict.dir,
                &prepared.branch,
                &conflict.pre_rebase_rev,
            )? {
                RebaseCheck::InProgress => still_pending.push(i),
                RebaseCheck::Resolved => {
                    writeln!(out, "{key}: conflicts resolved; {op} complete")?;
                    resolved[i] = true;
                }
                RebaseCheck::Aborted => {
                    writeln!(out, "{key}: {op} was aborted; handing back")?;
                }
                RebaseCheck::Incomplete => {
                    writeln!(
                        out,
                        "{key}: {op} state cleared without completing; handing back"
                    )?;
                }
            }
        }
        pending = still_pending;
        if pending.is_empty() {
            return Ok(resolved);
        }

        let now = deps.clock.now_unix_secs();
        if now - start > timeout {
            writeln!(out, "conflict resolution timed out after {timeout}s")?;
            writeln!(out, "attach: tmux attach -t {target}")?;
            for &i in &pending {
                let (prepared, conflict) = &tickets[i];
                writeln!(
                    out,
                    "abort {}: git -C {} {op} --abort",
                    prepared.key,
                    conflict.dir.display()
                )?;
            }
            return Ok(resolved);
        }

        let windows = deps.tmux.list_windows()?;
        if !has_live_window(&windows, target, window) {
            writeln!(out, "conflict session ended without completing every {op}")?;
            return Ok(resolved);
        }

        let mut count = 0;
        for &i in &pending {
            count += deps.git.conflicted_files(&tickets[i].1.dir)?.len();
        }
        let changed = last_count != Some(count);
        let heartbeat_due = now - last_heartbeat >= 60;
        if changed || heartbeat_due {
            writeln!(
                out,
                "[{}] resolving conflicts for {} tickets ({count} files)...",
                format_ts(now),
                pending.len()
            )?;
            last_heartbeat = now;
        }
        last_count = Some(count);

        deps.sleeper.sleep(CONFLICT_POLL_SECS);
    }
}

/// A rebase that stopped on conflicts during [`prepare_merge`], left in
/// progress on disk for a conflict session to finish.
#[derive(Debug, Clone)]
struct PendingConflict {
    /// The checkout the rebase is in progress in.
    dir: PathBuf,
    /// The branch tip before the rebase (or merge) started, after any
    /// squash (see [`check_rebase`]).
    pre_rebase_rev: String,
    /// Under [`MergeStrategy::Squash`], the branch tip before it was
    /// squashed, named in the hand-back message so the original commits
    /// can be restored (see [`write_restore_hint`]); `None` otherwise.
    restore_rev: Option<String>,
}

/// Everything [`prepare_merge`] learned about a ticket's PR, carried into
/// [`finish_merge`].
#[derive(Debug, Clone)]
struct PreparedMerge {
    /// The ticket key.
    key: String,
    /// The ticket's repo root.
    repo_root: PathBuf,
    /// The PR number.
    number: u64,
    /// The PR's head branch.
    branch: String,
    /// The PR's base branch.
    base: String,
    /// The PR's title, the message of a [`MergeStrategy::Squash`] commit.
    title: String,
    /// The local checkout of `branch`, if one was found.
    checkout_dir: Option<PathBuf>,
    /// Whether the local tip must be pushed before `gh pr merge`: with
    /// lease protection, except under [`MergeStrategy::Merge`] when the
    /// remote tip is still an ancestor (see [`PreparedMerge::needs_force`]).
    needs_push: bool,
    /// Whether that push must be forced even under
    /// [`MergeStrategy::Merge`]: set when the local branch was already
    /// rewritten (rebased) and diverged from its remote.
    needs_force: bool,
    /// `Some` when the rebase stopped on conflicts; the caller must get it
    /// resolved (and set `needs_push`) before calling [`finish_merge`].
    conflict: Option<PendingConflict>,
}

/// Stages 1-2 of the flow: resolve the PR, locate a checkout, fetch,
/// reconcile the local tip with its remote, and attempt the rebase onto
/// the base. Never opens a conflict session: a rebase that stops on
/// conflicts is returned in [`PreparedMerge::conflict`], still in
/// progress on disk.
fn prepare_merge(
    deps: &MergeDeps<'_>,
    key: &str,
    out: &mut dyn Write,
) -> Result<PreparedMerge, MergeError> {
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
    let title = pr.title.clone();
    writeln!(out, "found PR #{number} ({branch} -> {base})")?;
    ensure_base_not_red(deps, &repo_root, &base, out)?;

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

    // Tracks whether the local tip must be published before the merge: set
    // when local is authoritative (ahead of, or a rebase of, the remote
    // tip) or when a rebase runs below. The pushed tip is what `gh pr
    // merge` merges.
    let mut needs_push = false;
    let mut needs_force = false;
    let mut conflict = None;
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
                    needs_force = true;
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
                let squash_message = squash_message(&title, key, &branch);
                conflict =
                    integrate_base(deps, dir, &branch, &base, &squash_message, current_tip, out)?;
                if conflict.is_none() {
                    needs_push = true;
                }
            }
        }
        None => {
            writeln!(
                out,
                "warning: no local checkout of {branch}; skipping auto-{}",
                operation(deps.merge_cfg.strategy)
            )?;
        }
    }

    Ok(PreparedMerge {
        key: key.to_string(),
        repo_root,
        number,
        branch,
        base,
        title,
        checkout_dir,
        needs_push,
        needs_force,
        conflict,
    })
}

/// The message of a [`MergeStrategy::Squash`] commit: the PR's title, or
/// `<key>: <branch>` when the title is blank.
fn squash_message(title: &str, key: &str, branch: &str) -> String {
    if title.trim().is_empty() {
        format!("{key}: {branch}")
    } else {
        title.to_string()
    }
}

/// Brings the checkout at `dir` (on `branch`, whose tip is `tip`, with a
/// clean tree) up to date with `origin/<base>` the way
/// `[work.merge].strategy` says (GitHub issue #244):
///
/// - [`MergeStrategy::Rebase`]: `git rebase origin/<base>`.
/// - [`MergeStrategy::Squash`]: squash the branch's commits since its merge
///   base with `origin/<base>` into one commit with `squash_message`, then
///   rebase that commit, so every conflict comes up in one pass. Warns
///   (never refuses) when the repo merges PRs with a merge commit, since the
///   branch's commit history then never reaches the base.
/// - [`MergeStrategy::Merge`]: `git merge --no-ff origin/<base>` into the
///   branch, one conflict pass and no rewritten history.
///
/// Returns `None` once the branch is up to date, or the
/// [`PendingConflict`] left in progress on disk when it stopped on
/// conflicts.
fn integrate_base(
    deps: &MergeDeps<'_>,
    dir: &Path,
    branch: &str,
    base: &str,
    squash_message: &str,
    tip: String,
    out: &mut dyn Write,
) -> Result<Option<PendingConflict>, MergeError> {
    let strategy = deps.merge_cfg.strategy;
    let origin_base_ref = format!("origin/{base}");
    let mut pre_rebase_rev = tip.clone();
    let mut restore_rev = None;
    let outcome = match strategy {
        MergeStrategy::Merge => {
            writeln!(out, "merging {origin_base_ref} into {branch}...")?;
            deps.git.merge_into(dir, &origin_base_ref)?
        }
        MergeStrategy::Rebase | MergeStrategy::Squash => {
            if strategy == MergeStrategy::Squash {
                squash_branch(deps, dir, branch, base, squash_message, &tip, out)?;
                pre_rebase_rev = deps.git.rev_parse(dir, branch)?;
                restore_rev = Some(tip);
            }
            writeln!(out, "rebasing {branch} onto {origin_base_ref}...")?;
            deps.git.rebase_onto(dir, &origin_base_ref)?
        }
    };
    match outcome {
        RebaseOutcome::Completed => {
            writeln!(out, "{} complete", operation(strategy))?;
            Ok(None)
        }
        RebaseOutcome::Conflicted => Ok(Some(PendingConflict {
            dir: dir.to_path_buf(),
            pre_rebase_rev,
            restore_rev,
        })),
    }
}

/// The squash half of [`integrate_base`] under [`MergeStrategy::Squash`].
///
/// Logs `squashed <branch> (was <tip>)` as soon as the squash lands, so the
/// pre-squash sha is in the output whatever goes wrong afterwards: an error
/// after this point leaves the branch squashed and diverged from its
/// remote, and only that sha (or the reflog) restores it.
fn squash_branch(
    deps: &MergeDeps<'_>,
    dir: &Path,
    branch: &str,
    base: &str,
    message: &str,
    tip: &str,
    out: &mut dyn Write,
) -> Result<(), MergeError> {
    if let Ok(Some(RepoMergeMethod::MergeCommit)) = deps.gh.repo_merge_method(dir, MERGE_TIMEOUT) {
        writeln!(
            out,
            "warning: this repo merges PRs with a merge commit; squashing {branch} drops its commit history from {base}"
        )?;
    }
    let since = deps
        .git
        .merge_base(dir, branch, &format!("origin/{base}"))?;
    writeln!(out, "squashing {branch} into one commit...")?;
    deps.git.squash_since(dir, &since, message)?;
    writeln!(out, "squashed {branch} (was {tip})")?;
    Ok(())
}

/// Publishes `branch` from `dir`: with lease protection when `force`, else
/// a plain push that only fast-forwards the remote.
fn push_branch(
    deps: &MergeDeps<'_>,
    dir: &Path,
    branch: &str,
    force: bool,
    out: &mut dyn Write,
) -> Result<(), MergeError> {
    if force {
        writeln!(out, "pushing {branch} (force-with-lease)...")?;
        deps.git.push_force_with_lease(dir, branch)?;
    } else {
        writeln!(out, "pushing {branch}...")?;
        deps.git.push(dir, branch)?;
    }
    Ok(())
}

/// Stages 3-7 of the flow for a ticket whose rebase (if any) is complete:
/// run `[work.merge] verify_command` and push the local tip when
/// [`PreparedMerge::needs_push`] says to (see [`verify_and_push`]), wait
/// for the PR's checks to pass on its head (see [`wait_for_checks`];
/// [`MergeFlowOutcome::ChecksPending`] when they outlast the timeout),
/// `gh pr merge` (rebasing and retrying once on a stale-base rejection, see
/// [`merge_pr_with_retry`] — the only way this returns
/// [`MergeFlowOutcome::ConflictsHandedBack`], before any cleanup), sync
/// the local base, best-effort clean up (worktree, local
/// branch, the ticket's live runs via [`finish_ticket_runs`], then its tmux
/// session via [`session_gc::archive_and_kill`]), and apply
/// `status_on_merge`, then sweep stale status labels off a closed ticket
/// and close out stacked children.
///
/// `kept_session` names a tmux session that must survive this ticket's
/// cleanup even if it is the ticket's own: a batch's shared conflict
/// session while another ticket is still handed back to it.
fn finish_merge(
    deps: &MergeDeps<'_>,
    prepared: &PreparedMerge,
    kept_session: Option<&str>,
    out: &mut dyn Write,
) -> Result<MergeFlowOutcome, MergeError> {
    let PreparedMerge {
        key,
        repo_root,
        number,
        branch,
        base,
        checkout_dir,
        needs_push,
        needs_force,
        conflict: _,
        title: _,
    } = prepared;
    let (key, number, needs_push) = (key.as_str(), *number, *needs_push);

    let mut pushed = None;
    if needs_push && let Some(dir) = checkout_dir.as_deref() {
        let force = deps.merge_cfg.strategy != MergeStrategy::Merge || *needs_force;
        pushed = Some(verify_and_push(deps, prepared, dir, force, out)?);
    }
    if wait_for_checks(deps, prepared, pushed.as_ref(), out)? == ChecksVerdict::TimedOut {
        return Ok(MergeFlowOutcome::ChecksPending);
    }

    let merge_outcome = merge_pr_with_retry(deps, prepared, out)?;
    if merge_outcome != MergeFlowOutcome::Merged {
        return Ok(merge_outcome);
    }

    writeln!(out, "syncing local {base}...")?;
    deps.git.fetch_origin(repo_root)?;
    let repo_root_on_base =
        deps.git.current_branch(repo_root).ok().as_deref() == Some(base.as_str());
    if repo_root_on_base {
        if deps.git.status_is_clean(repo_root)? {
            deps.git
                .merge_ff_only(repo_root, &format!("origin/{base}"))?;
            writeln!(out, "local {base} synced")?;
        } else {
            writeln!(
                out,
                "warning: local {base} checkout is dirty; skipped fast-forward (run: git -C {} merge --ff-only origin/{base})",
                repo_root.display()
            )?;
        }
    } else {
        match deps.git.fetch_branch_to_local(repo_root, base) {
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
            match deps.git.remove_worktree(repo_root, dir) {
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
        .branch_exists_local(repo_root, branch)
        .unwrap_or(false);
    if branch_exists {
        if skip_branch_due_to_worktree {
            writeln!(
                out,
                "warning: skipped deleting local branch {branch}; its worktree removal was skipped (run: git -C {} branch -D {branch} once the worktree is removed)",
                repo_root.display()
            )?;
        } else {
            let local_rev = deps.git.rev_parse(repo_root, branch);
            let remote_rev = deps.git.rev_parse(repo_root, &format!("origin/{branch}"));
            match (local_rev, remote_rev) {
                (Ok(l), Ok(r)) if l == r => match deps.git.delete_branch(repo_root, branch) {
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

    finish_ticket_runs(deps, key, out)?;
    cleanup_ticket_session(deps, key, kept_session, out)?;

    if let Some(target_status) = deps.status_on_merge {
        match apply_status_on_merge(deps.jira, key, target_status) {
            StatusTransition::Applied(status) => writeln!(out, "moved {key} to {status}")?,
            StatusTransition::AlreadyInStatus(status) => {
                writeln!(out, "{key} already in {status}")?
            }
            StatusTransition::Warning(warning) => writeln!(out, "warning: {warning}")?,
        }
    }

    // Last, so a closing keyword GitHub applies asynchronously after the
    // merge has had the whole sync/cleanup above to land (GitHub issue #75).
    match deps.jira.clear_closed_status_labels(key) {
        Ok(removed) => {
            for label in removed {
                writeln!(out, "cleared stale status label {label} from {key}")?;
            }
        }
        Err(err) => writeln!(
            out,
            "warning: failed to clear stale status labels from {key}: {err} (run: tm backend clean-status-labels)"
        )?,
    }

    reconcile_stacked_children(deps, repo_root, branch, kept_session, out)?;

    writeln!(out, "merged PR #{number} for {key}; local {base} synced")?;
    Ok(MergeFlowOutcome::Merged)
}

/// The head [`verify_and_push`] published, which [`wait_for_checks`]
/// requires the PR's checks to have run on.
#[derive(Debug, Clone)]
struct PushedHead {
    /// The pushed commit. Empty when the checks gate is off.
    sha: String,
    /// Whether the PR had checks before the push. If so, an empty check
    /// list on the pushed head means CI hasn't queued them yet, not that
    /// the repo has no CI.
    expect_checks: bool,
}

/// Runs `[work.merge] verify_command` (if set) in `dir`, then pushes the
/// branch through [`push_branch`] (GitHub issue #241), with lease
/// protection when `force`. A failing command stops the
/// merge before anything is pushed. With the checks gate on, it first notes
/// whether the PR has checks at all, and afterwards which commit was
/// pushed, for [`wait_for_checks`].
fn verify_and_push(
    deps: &MergeDeps<'_>,
    prepared: &PreparedMerge,
    dir: &Path,
    force: bool,
    out: &mut dyn Write,
) -> Result<PushedHead, MergeError> {
    let branch = &prepared.branch;
    if let Some(command) = deps.merge_cfg.verify_command.as_deref() {
        let spawn_err = |source| MergeError::VerifySpawn {
            command: command.to_string(),
            source,
        };
        std::fs::create_dir_all(deps.state_dir).map_err(spawn_err)?;
        let log = verify_log_path(deps.state_dir, &prepared.key);
        writeln!(
            out,
            "running verify_command `{command}` in {} (output in {})...",
            dir.display(),
            log.display()
        )?;
        out.flush()?;
        let passed = (deps.run_verify)(dir, command, &log).map_err(spawn_err)?;
        if !passed {
            return Err(MergeError::VerifyFailed {
                command: command.to_string(),
                dir: dir.to_path_buf(),
                log,
            });
        }
        writeln!(out, "verify_command passed")?;
    }

    let gate = deps.merge_cfg.wait_for_checks;
    // A failed read only warns, like a failed poll in [`wait_for_checks`]:
    // assuming checks exist at worst waits out the timeout, where failing
    // here would throw away a verify_command run that already passed.
    let expect_checks = gate
        && match deps.gh.pr_checks(&prepared.repo_root, prepared.number) {
            Ok(checks) => !checks.checks.is_empty(),
            Err(err) => {
                writeln!(
                    out,
                    "warning: could not read checks before pushing: {err}; waiting for checks on the pushed head"
                )?;
                true
            }
        };
    push_branch(deps, dir, branch, force, out)?;
    let sha = if gate {
        deps.git.rev_parse(dir, branch)?
    } else {
        String::new()
    };
    Ok(PushedHead { sha, expect_checks })
}

/// Where [`verify_and_push`] writes `key`'s `verify_command` output: one
/// file per ticket under `state_dir`, overwritten by each run.
fn verify_log_path(state_dir: &Path, key: &str) -> PathBuf {
    state_dir.join(format!("merge-{key}-verify.log"))
}

/// How [`wait_for_checks`] ended, short of a failing check (an error).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChecksVerdict {
    /// Every check passed, or the PR has none: safe to merge.
    Passed,
    /// Checks were still running when the timeout ran out.
    TimedOut,
}

/// One reading of a PR's checks against what [`wait_for_checks`] needs.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ChecksReading {
    Passed,
    Failed(Vec<String>),
    /// Not finished yet; the text says what is being waited on.
    Waiting(String),
}

/// Reads `checks` against `pushed` (the head the checks must have run on,
/// `None` when nothing was pushed and the PR's current head stands).
fn read_checks(checks: &PrChecks, pushed: Option<&PushedHead>) -> ChecksReading {
    if let Some(head) = pushed
        && checks.head_sha != head.sha
    {
        return ChecksReading::Waiting(format!(
            "waiting for GitHub to see the pushed head {}",
            short_sha(&head.sha)
        ));
    }
    let failing: Vec<String> = checks.failing().map(|check| check.name.clone()).collect();
    if !failing.is_empty() {
        return ChecksReading::Failed(failing);
    }
    if checks.checks.is_empty() {
        return if pushed.is_some_and(|head| head.expect_checks) {
            ChecksReading::Waiting("waiting for checks to start on the pushed head".to_string())
        } else {
            ChecksReading::Passed
        };
    }
    let pending = checks
        .checks
        .iter()
        .filter(|check| check.state == ChecksState::Pending)
        .count();
    if pending == 0 {
        ChecksReading::Passed
    } else {
        ChecksReading::Waiting(format!(
            "waiting for checks: {pending} of {} still running",
            checks.checks.len()
        ))
    }
}

/// The checks gate before `gh pr merge` (GitHub issue #241): polls the PR's
/// checks every [`CHECKS_POLL_SECS`] until every check on its head has
/// passed (or it has none), so `tm merge` never merges a commit CI hasn't
/// tested. After a push, `pushed` names the new head: checks GitHub still
/// reports for the old one don't count. A failing check is
/// [`MergeError::ChecksFailed`]; checks still running after
/// `[work.merge] checks_timeout_secs` are [`ChecksVerdict::TimedOut`]. A
/// `gh` error while polling only warns, so one network blip doesn't
/// throw away a long wait. `[work.merge] wait_for_checks = false` skips
/// the gate.
fn wait_for_checks(
    deps: &MergeDeps<'_>,
    prepared: &PreparedMerge,
    pushed: Option<&PushedHead>,
    out: &mut dyn Write,
) -> Result<ChecksVerdict, MergeError> {
    if !deps.merge_cfg.wait_for_checks {
        return Ok(ChecksVerdict::Passed);
    }
    let number = prepared.number;
    let started = deps.clock.now_unix_secs();
    let timeout = i64::try_from(deps.merge_cfg.checks_timeout_secs).unwrap_or(i64::MAX);
    let mut last_line: Option<String> = None;
    let mut last_printed = started;
    loop {
        let line = match deps.gh.pr_checks(&prepared.repo_root, number) {
            Ok(checks) => match read_checks(&checks, pushed) {
                ChecksReading::Passed => {
                    if !checks.checks.is_empty() {
                        writeln!(out, "checks passed on PR #{number}")?;
                    }
                    return Ok(ChecksVerdict::Passed);
                }
                ChecksReading::Failed(names) => {
                    return Err(MergeError::ChecksFailed {
                        number,
                        checks: names,
                    });
                }
                ChecksReading::Waiting(line) => format!("PR #{number}: {line}"),
            },
            Err(err) => format!("warning: failed to read checks on PR #{number}: {err}"),
        };

        let now = deps.clock.now_unix_secs();
        let elapsed = now.saturating_sub(started);
        if last_line.as_deref() != Some(line.as_str())
            || now.saturating_sub(last_printed) >= CHECKS_HEARTBEAT_SECS
        {
            writeln!(out, "{line} ({elapsed}s elapsed)")?;
            out.flush()?;
            last_printed = now;
            last_line = Some(line);
        }
        if elapsed >= timeout {
            writeln!(
                out,
                "timed out after {elapsed}s waiting for checks on PR #{number}; not merged (rerun `tm merge {}` once they finish)",
                prepared.key
            )?;
            return Ok(ChecksVerdict::TimedOut);
        }
        deps.sleeper.sleep(CHECKS_POLL_SECS);
    }
}

/// The `gh pr merge` step of [`finish_merge`], retried once when GitHub
/// rejects the merge because the base moved (GitHub issue #109; see
/// [`GhError::is_out_of_date_base`]) — typically an earlier ticket in the
/// same batch merging first, which branch protection's "require branches
/// to be up to date" then refuses.
///
/// The retry fetches, rebases the checkout onto the current
/// `origin/<base>`, verifies and force-pushes with lease
/// ([`verify_and_push`]), waits for the checks on the new head
/// ([`wait_for_checks`]), and merges again. A rebase
/// that stops on conflicts goes through [`run_conflict_session`] like the
/// first rebase did; if that session never resolves, the rebase is left
/// in progress on disk and [`MergeFlowOutcome::ConflictsHandedBack`] is
/// returned without merging. Without a local checkout there is nothing to
/// rebase, so the rejection is returned as-is; any other rejection, or a
/// second one after the retry, is an ordinary error.
fn merge_pr_with_retry(
    deps: &MergeDeps<'_>,
    prepared: &PreparedMerge,
    out: &mut dyn Write,
) -> Result<MergeFlowOutcome, MergeError> {
    let PreparedMerge {
        key,
        repo_root,
        number,
        branch,
        base,
        title,
        checkout_dir,
        ..
    } = prepared;

    writeln!(out, "merging PR #{number}...")?;
    let rejection = match deps.gh.pr_merge(repo_root, *number, MERGE_TIMEOUT) {
        Ok(()) => return Ok(MergeFlowOutcome::Merged),
        Err(err) => err,
    };
    let Some(dir) = checkout_dir
        .as_deref()
        .filter(|_| rejection.is_out_of_date_base())
    else {
        return Err(rejection.into());
    };

    writeln!(
        out,
        "PR #{number} rejected: {branch} is behind {base}; {} and retrying once",
        match deps.merge_cfg.strategy {
            MergeStrategy::Merge => "merging",
            MergeStrategy::Rebase | MergeStrategy::Squash => "rebasing",
        }
    )?;
    deps.git.fetch_origin(dir)?;
    let pre_rebase_rev = deps.git.rev_parse(dir, branch)?;
    if !deps.git.status_is_clean(dir)? {
        return Err(MergeError::DirtyWorktree {
            dir: dir.to_path_buf(),
        });
    }
    let squash_message = squash_message(title, key, branch);
    if let Some(conflict) = integrate_base(
        deps,
        dir,
        branch,
        base,
        &squash_message,
        pre_rebase_rev,
        out,
    )? {
        match run_conflict_session(deps, prepared, &conflict, out)? {
            ConflictOutcome::Resolved => {}
            ConflictOutcome::HandedBack => return Ok(MergeFlowOutcome::ConflictsHandedBack),
        }
    }

    let force = deps.merge_cfg.strategy != MergeStrategy::Merge;
    let pushed = verify_and_push(deps, prepared, dir, force, out)?;
    if wait_for_checks(deps, prepared, Some(&pushed), out)? == ChecksVerdict::TimedOut {
        return Ok(MergeFlowOutcome::ChecksPending);
    }
    writeln!(out, "retrying merge of PR #{number}...")?;
    deps.gh.pr_merge(repo_root, *number, MERGE_TIMEOUT)?;
    Ok(MergeFlowOutcome::Merged)
}

/// The stacked-children step of [`finish_merge`] (GitHub issue #94): move
/// the ticket of every PR already merged into `branch`, and transitively
/// into those PRs' branches, to `status_on_merge` (else `Done`, the target
/// `tm drift --fix` suggests), then sweep its stale status labels.
///
/// GitHub only honours a closing keyword on a PR into the default branch,
/// so a stacked PR merged into its blocker's branch outside `tm merge`
/// never moves its ticket; merging the blocker is when that work lands —
/// so each child's runs and session are closed out here too, exactly as
/// the merged ticket's are (GitHub issue #107). Advisory like the rest of
/// the cleanup: never fails the merge.
fn reconcile_stacked_children(
    deps: &MergeDeps<'_>,
    repo_root: &Path,
    branch: &str,
    kept_session: Option<&str>,
    out: &mut dyn Write,
) -> Result<(), MergeError> {
    let target = deps.status_on_merge.unwrap_or("Done");
    let mut visited = std::collections::HashSet::new();
    let mut pending = vec![branch.to_string()];
    while let Some(base) = pending.pop() {
        if !visited.insert(base.clone()) {
            continue;
        }
        let children = match deps.gh.pr_list_merged_into(repo_root, &base) {
            Ok(children) => children,
            Err(err) => {
                writeln!(
                    out,
                    "warning: failed to list PRs merged into {base}: {err} (run: tm drift)"
                )?;
                continue;
            }
        };
        for child in children {
            pending.push(child.head_ref_name.clone());
            let number = child.number;
            let Some(key) = find_issue_key(&child, &|token| deps.jira.is_ticket_key(token)) else {
                continue;
            };
            match reconcile_status(deps.jira, &key, target) {
                StatusTransition::Applied(status) => {
                    writeln!(out, "moved stacked {key} (PR #{number}) to {status}")?
                }
                StatusTransition::AlreadyInStatus(status) => {
                    writeln!(out, "stacked {key} (PR #{number}) already in {status}")?
                }
                StatusTransition::Warning(warning) => writeln!(out, "warning: {warning}")?,
            }
            if let Err(err) = deps.jira.clear_closed_status_labels(&key) {
                writeln!(
                    out,
                    "warning: failed to clear stale status labels from {key}: {err} (run: tm backend clean-status-labels)"
                )?;
            }
            finish_ticket_runs(deps, &key, out)?;
            cleanup_ticket_session(deps, &key, kept_session, out)?;
        }
    }
    Ok(())
}

/// The run close-out step of [`finish_merge`] (GitHub issue #107): finish
/// every live (`queued`, `running`, `hibernated`) run recorded for `key`
/// as `done`, so [`session_gc::archive_and_kill`]'s live-run guard stops
/// holding the merged ticket's session open — only another ticket's live
/// run hosted there still does. The ticket's latest `review-watch` run also
/// gets a `pr_closed` event via [`close_out_watch`] (GitHub issue #119).
///
/// A `running` run whose recorded pid is still alive has that agent
/// stopped via [`MergeDeps::kill_pid`], after its row is marked done (the
/// order idle hibernation uses, so nothing reads the dying process as a
/// crash). A hibernated run's agent is already gone, so its pid — possibly
/// reused since — is never signalled. When tm is itself running inside
/// `key`'s session (or that cannot be told), no agent is stopped: the one
/// hosting tm is likely among them. Never fails the merge; only writing to
/// `out` can error.
fn finish_ticket_runs(
    deps: &MergeDeps<'_>,
    key: &str,
    out: &mut dyn Write,
) -> Result<(), MergeError> {
    use crate::runs::{FinishRun, RunStatus};

    let Some(store) = deps.run_store else {
        return Ok(());
    };
    let runs = match store.runs_for_ticket(Some(&deps.identity.scope()), key) {
        Ok(runs) => runs,
        Err(err) => {
            writeln!(
                out,
                "warning: failed to read {key}'s runs: {err}; left them unfinished (run: tm runs finish <id> --status done)"
            )?;
            return Ok(());
        }
    };
    if let Some(watch) = runs.iter().rev().find(|run| run.kind == "review-watch") {
        close_out_watch(store, watch.id, key, out)?;
    }
    let live: Vec<_> = runs
        .into_iter()
        .filter(|run| {
            matches!(
                run.status,
                RunStatus::Queued | RunStatus::Running | RunStatus::Hibernated
            )
        })
        .collect();
    if live.is_empty() {
        return Ok(());
    }

    let session = ticket_session_name(&deps.identity.session_slug(), key);
    let inside_session = !matches!(
        deps.tmux.current_session_name(),
        Ok(ref current) if current.as_deref() != Some(session.as_str())
    );
    let done = FinishRun {
        status: RunStatus::Done,
        ..Default::default()
    };
    for run in live {
        if let Err(err) = store.finish_run(run.id, &done) {
            writeln!(
                out,
                "warning: failed to finish run {} ({} {key}): {err} (run: tm runs finish {} --status done)",
                run.id, run.kind, run.id
            )?;
            continue;
        }
        writeln!(out, "finished run {} ({} {key}) as done", run.id, run.kind)?;
        if run.status != RunStatus::Running {
            continue;
        }
        let Some(pid) = run.pid.filter(|pid| pid_alive(*pid)) else {
            continue;
        };
        if inside_session {
            writeln!(
                out,
                "warning: run {} left its agent (pid {pid}) running; tm is running inside {session} (run: kill {pid} once you leave it)",
                run.id
            )?;
        } else {
            (deps.kill_pid)(pid);
        }
    }
    Ok(())
}

/// Records the `pr_closed` event `tm pr watch` records when it sees its PR
/// merged on watch run `run_id`, unless the watch already did (GitHub issue
/// #119). A watch that finished before the merge never sees it, and the
/// overview reads a finished watch with no `pr_closed` as
/// [`crate::runs::overview::Stage::AwaitingReview`]. Never fails the merge;
/// only writing to `out` can error.
fn close_out_watch(
    store: &crate::runs::RunStore,
    run_id: i64,
    key: &str,
    out: &mut dyn Write,
) -> Result<(), MergeError> {
    let recorded = store
        .events_for_run(run_id)
        .map(|events| events.iter().any(|event| event.kind == "pr_closed"))
        .and_then(|closed| {
            if !closed {
                store.add_event(run_id, "pr_closed", Some(r#"{"reason":"merged"}"#))?;
            }
            Ok(())
        });
    if let Err(err) = recorded {
        writeln!(
            out,
            "warning: failed to record {key}'s merge on watch run {run_id}: {err}; tm overview may still show it awaiting review"
        )?;
    }
    Ok(())
}

/// The ticket-session step of [`finish_merge`]'s best-effort cleanup
/// (GitHub issue #78): archive `key`'s `tm-<scope>-<key>` session's
/// scrollback under `<state_dir>/archive`, then kill it — unless it is
/// `kept_session`, or [`session_gc::archive_and_kill`] finds a reason to
/// leave it. Never fails the merge; only writing to `out` can error.
fn cleanup_ticket_session(
    deps: &MergeDeps<'_>,
    key: &str,
    kept_session: Option<&str>,
    out: &mut dyn Write,
) -> Result<(), MergeError> {
    let slug = deps.identity.session_slug();
    let session = ticket_session_name(&slug, key);
    if kept_session == Some(session.as_str()) {
        writeln!(
            out,
            "warning: left tmux session {session} running; it hosts the conflict session for handed-back tickets (run: tmux kill-session -t {session} once they are merged)"
        )?;
        return Ok(());
    }
    let archive_root = session_gc::archive_root(deps.state_dir);
    let gc = session_gc::SessionGcDeps {
        tmux: deps.tmux,
        store: deps.run_store,
        archive_root: &archive_root,
        pid_alive: &pid_alive,
        now_unix_secs: deps.clock.now_unix_secs(),
    };
    session_gc::archive_and_kill(&gc, &deps.identity.scope(), &slug, key, out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::claude::ClaudeRunner;
    use crate::github::gh_cli::{CheckResult, CommitChecks, FakeGhCli};
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
                parent: None,
                labels: Vec::new(),
                issue_type: None,
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
        work_default_model: Option<String>,
        models: crate::config::ModelsConfig,
        status_on_merge: Option<String>,
        allow_red_base: bool,
        _worktree_dirs: Vec<tempfile::TempDir>,
        _state_dir: tempfile::TempDir,
        state_dir: PathBuf,
        _sessions_tmp: tempfile::TempDir,
        sessions_dir: PathBuf,
        /// Every pid [`MergeDeps::kill_pid`] was asked to stop, in order.
        killed_pids: std::rc::Rc<std::cell::RefCell<Vec<u32>>>,
        kill_pid: Box<dyn Fn(u32)>,
        /// Every `(dir, command)` [`MergeDeps::run_verify`] was asked to
        /// run, in order.
        verify_calls: std::rc::Rc<std::cell::RefCell<Vec<(PathBuf, String)>>>,
        /// What the fake verify command reports: `true` passes.
        verify_passes: std::rc::Rc<std::cell::Cell<bool>>,
        run_verify: Box<VerifyRunner>,
    }

    impl Fixture {
        fn new() -> Self {
            let db_dir = tempdir().unwrap();
            let store = open_store(db_dir.path());
            let state_tmp = tempdir().unwrap();
            let state_dir = state_tmp.path().join("state");
            let sessions_tmp = tempdir().unwrap();
            let sessions_dir = sessions_tmp.path().join("sessions");
            let killed_pids = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let recorder = killed_pids.clone();
            let verify_calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let verify_passes = std::rc::Rc::new(std::cell::Cell::new(true));
            let (verify_recorder, verify_verdict) = (verify_calls.clone(), verify_passes.clone());
            Fixture {
                killed_pids,
                kill_pid: Box::new(move |pid| recorder.borrow_mut().push(pid)),
                verify_calls,
                verify_passes,
                run_verify: Box::new(move |dir, command, _log| {
                    verify_recorder
                        .borrow_mut()
                        .push((dir.to_path_buf(), command.to_string()));
                    Ok(verify_verdict.get())
                }),
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
                // Off by default here so the post-merge memory session's
                // window doesn't show up in every test's tmux calls; the
                // GitHub issue #208 tests turn it back on.
                // The checks gate is off here too, so existing tests don't
                // need a `pr_checks` result matching every pushed head; the
                // GitHub issue #241 tests turn it back on.
                merge_cfg: MergeConfig {
                    consolidate_memories: false,
                    wait_for_checks: false,
                    ..MergeConfig::default()
                },
                work_default_model: None,
                models: crate::config::ModelsConfig::default(),
                status_on_merge: None,
                allow_red_base: false,
                _worktree_dirs: Vec::new(),
                _state_dir: state_tmp,
                state_dir,
                _sessions_tmp: sessions_tmp,
                sessions_dir,
            }
        }

        /// Registers a `lane`-kind run row for `PROJ-1` at a fresh temp
        /// directory checked out on `branch`, and configures the fake to
        /// report that branch for the directory — the checkout-resolution
        /// candidate most tests that need a real rebase to run exercise.
        /// Keeps `repo_root` ("/repo", a path that doesn't exist on disk)
        /// free to represent the base branch's checkout in the same test.
        fn register_checkout(&mut self, branch: &str) -> PathBuf {
            self.register_checkout_for("PROJ-1", branch)
        }

        /// [`Self::register_checkout`] for an arbitrary ticket `key` — the
        /// batch tests register one checkout per ticket.
        fn register_checkout_for(&mut self, key: &str, branch: &str) -> PathBuf {
            let dir = tempdir().unwrap();
            let path = dir.path().to_path_buf();
            self.store
                .start_run(&StartRun {
                    scope: self.identity.scope(),
                    ticket: key.to_string(),
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
                admission: crate::work::admission::AdmissionDeps::off(),
                model_defaults: crate::work::model::ModelDefaults {
                    work_default: self.work_default_model.as_deref(),
                    models: &self.models,
                },
                status_on_merge: self.status_on_merge.as_deref(),
                state_dir: &self.state_dir,
                sessions_dir: &self.sessions_dir,
                kill_pid: &*self.kill_pid,
                allow_red_base: self.allow_red_base,
                run_verify: &*self.run_verify,
            }
        }
    }

    fn run(deps: &MergeDeps<'_>, key: &str) -> (Result<MergeFlowOutcome, MergeError>, String) {
        let mut out = Vec::new();
        let result = run_merge(deps, key, &mut out);
        (result, String::from_utf8(out).unwrap())
    }

    // --- red base gate (GitHub issue #242) ---

    fn check(name: &str, state: crate::github::gh_cli::ChecksState) -> CheckResult {
        CheckResult {
            name: name.to_string(),
            state,
            run_id: None,
            job_id: None,
            details_url: None,
            conclusion: None,
        }
    }

    fn red_main() -> CommitChecks {
        use crate::github::gh_cli::ChecksState;
        CommitChecks {
            sha: "0123456789abcdef".to_string(),
            checks: vec![
                check("test", ChecksState::Failing),
                check("lint", ChecksState::Passing),
                check("build", ChecksState::Failing),
            ],
        }
    }

    #[test]
    fn red_base_refuses_before_fetching_and_names_the_commit_and_checks() {
        let mut fx = up_to_date_fixture();
        fx.gh = std::mem::replace(&mut fx.gh, FakeGhCli::new())
            .with_commit_checks("main", Ok(red_main()));

        let (result, _out) = run(&fx.deps(), "PROJ-1");

        let err = result.unwrap_err();
        assert!(matches!(err, MergeError::BaseRed { .. }), "{err:?}");
        let message = err.to_string();
        assert!(message.contains("main"), "{message}");
        assert!(message.contains("0123456789ab"), "{message}");
        assert!(message.contains("test, build"), "{message}");
        assert!(!message.contains("lint"), "{message}");
        assert!(message.contains("--allow-red-base"), "{message}");
        assert_eq!(fx.gh.commit_checks_calls(), vec!["main".to_string()]);
        assert!(fx.git.call_log().is_empty(), "{:?}", fx.git.call_log());
        assert!(fx.gh.pr_merge_calls().is_empty());
    }

    #[test]
    fn allow_red_base_skips_the_gate() {
        let mut fx = up_to_date_fixture();
        fx.gh = std::mem::replace(&mut fx.gh, FakeGhCli::new())
            .with_commit_checks("main", Ok(red_main()));
        fx.allow_red_base = true;

        let (result, _out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)), "{result:?}");
        assert!(fx.gh.commit_checks_calls().is_empty());
        assert_eq!(fx.gh.pr_merge_calls(), vec![(PathBuf::from("/repo"), 7)]);
    }

    #[test]
    fn pending_base_proceeds() {
        use crate::github::gh_cli::ChecksState;
        let mut fx = up_to_date_fixture();
        fx.gh = std::mem::replace(&mut fx.gh, FakeGhCli::new()).with_commit_checks(
            "main",
            Ok(CommitChecks {
                sha: "abc".to_string(),
                checks: vec![
                    check("test", ChecksState::Pending),
                    check("lint", ChecksState::Passing),
                ],
            }),
        );

        let (result, _out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)), "{result:?}");
        assert_eq!(fx.gh.commit_checks_calls(), vec!["main".to_string()]);
    }

    #[test]
    fn unreadable_base_checks_warn_and_proceed() {
        let mut fx = up_to_date_fixture();
        fx.gh = std::mem::replace(&mut fx.gh, FakeGhCli::new()).with_commit_checks(
            "main",
            Err(GhError::Command {
                command: "gh api graphql".to_string(),
                exit_code: Some(1),
                stderr: "HTTP 502".to_string(),
            }),
        );

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)), "{result:?}");
        assert!(
            out.contains("warning: could not read main's checks"),
            "{out}"
        );
    }

    #[test]
    fn batch_records_a_red_base_ticket_as_failed() {
        let mut fx = up_to_date_fixture();
        fx.gh = std::mem::replace(&mut fx.gh, FakeGhCli::new())
            .with_commit_checks("main", Ok(red_main()));

        let (result, _out) = run_batch(&fx.deps(), &["PROJ-1".to_string()]);

        let report = result.unwrap();
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, "PROJ-1");
        assert!(report.failed[0].1.contains("--allow-red-base"));
        assert!(fx.gh.pr_merge_calls().is_empty());
    }

    // --- conflict_prompt ---

    #[test]
    fn conflict_prompt_substitutes_every_placeholder() {
        let rendered = conflict_prompt(
            "resolve {key} on {branch} onto {base}\n{upstream_context}",
            "PROJ-1",
            "proj-1-fix",
            "main",
            "- PR #12: Split config",
        );
        assert_eq!(
            rendered,
            "resolve PROJ-1 on proj-1-fix onto main\n- PR #12: Split config"
        );
    }

    // --- upstream context ---

    fn commit_pull(number: u64, title: &str, body: &str) -> CommitPull {
        CommitPull {
            number,
            title: title.to_string(),
            body: body.to_string(),
        }
    }

    #[test]
    fn render_upstream_context_lists_each_pr_with_its_indented_body() {
        let rendered = render_upstream_context(
            &[
                commit_pull(12, "Split config", "Why:\nit grew"),
                commit_pull(13, "No body", ""),
            ],
            UPSTREAM_CONTEXT_LIMIT_BYTES,
        );

        assert_eq!(
            rendered,
            "Pull requests merged into the base that changed the conflicted files:\n\n\
             - PR #12: Split config\n  Why:\n  it grew\n\n\
             - PR #13: No body\n"
        );
    }

    #[test]
    fn render_upstream_context_trims_bodies_to_share_the_size_cap() {
        let long = "x".repeat(10_000);
        let pulls = [commit_pull(1, "a", &long), commit_pull(2, "b", &long)];

        let rendered = render_upstream_context(&pulls, 2_000);

        assert!(rendered.len() < 2_500, "{}", rendered.len());
        assert_eq!(
            rendered.matches("bytes trimmed ...]").count(),
            2,
            "{rendered}"
        );
    }

    #[test]
    fn render_upstream_context_with_no_pulls_says_so() {
        assert_eq!(
            render_upstream_context(&[], UPSTREAM_CONTEXT_LIMIT_BYTES),
            NO_UPSTREAM_PRS
        );
    }

    #[test]
    fn conflict_prompt_lists_the_upstream_prs_that_touched_the_conflicted_files() {
        let mut fx = resolving_conflict_fixture();
        fx.merge_cfg.prompt = Some("CONTEXT\n{upstream_context}".to_string());
        fx.git = std::mem::take(&mut fx.git)
            .with_commits_touching_result(Ok(vec!["s1".to_string(), "s2".to_string()]));
        fx.gh = std::mem::take(&mut fx.gh)
            .with_commit_pulls("s1", Ok(vec![commit_pull(12, "Split config", "Why")]))
            .with_commit_pulls(
                "s2",
                Ok(vec![
                    commit_pull(12, "Split config", "Why"),
                    commit_pull(13, "Rename section", ""),
                ]),
            );

        let (result, _out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)));
        assert_eq!(
            fx.git.commits_touching_calls(),
            vec![(
                "aaa".to_string(),
                "origin/main".to_string(),
                vec!["a.rs".to_string()]
            )]
        );
        let prompt = conflict_prompt_file(&fx, "PROJ-1");
        assert_eq!(
            prompt.matches("PR #12: Split config").count(),
            1,
            "{prompt}"
        );
        assert!(prompt.contains("PR #13: Rename section"), "{prompt}");
        assert!(!prompt.contains("{upstream_context}"), "{prompt}");
    }

    #[test]
    fn conflict_session_still_opens_when_the_upstream_lookup_fails() {
        let mut fx = resolving_conflict_fixture();
        fx.merge_cfg.prompt = Some("CONTEXT\n{upstream_context}".to_string());
        fx.git =
            std::mem::take(&mut fx.git).with_commits_touching_result(Ok(vec!["s1".to_string()]));
        fx.gh = std::mem::take(&mut fx.gh).with_commit_pulls(
            "s1",
            Err(GhError::Command {
                command: "gh api commits/pulls".to_string(),
                exit_code: Some(1),
                stderr: "HTTP 502".to_string(),
            }),
        );

        let (result, _out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)));
        let prompt = conflict_prompt_file(&fx, "PROJ-1");
        assert!(
            prompt.contains("Upstream pull request context is unavailable"),
            "{prompt}"
        );
        assert!(prompt.contains("HTTP 502"), "{prompt}");
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

    // --- conflict session run rows (GitHub issue #156) ---

    /// A single-ticket conflict inside tmux session `dev` that stays in
    /// progress for two poll ticks (so the poll loop sleeps twice) and then
    /// resolves.
    fn resolving_conflict_fixture() -> Fixture {
        let mut fx = Fixture::new();
        fx.gh = fx
            .gh
            .with_pr_list(Ok(vec![pr_info(7, "proj-1-fix", "main", "PROJ-1")]));
        fx.git = fx
            .git
            .with_current_branch(Ok("proj-1-fix".to_string()))
            .with_current_branch_for(PathBuf::from("/repo"), Ok("proj-1-fix".to_string()))
            .with_rev_parse_sequence(
                "proj-1-fix",
                vec![Ok("aaa".to_string()), Ok("bbb".to_string())],
            )
            .with_rev_parse_result("origin/proj-1-fix", Ok("aaa".to_string()))
            .with_is_ancestor_result(Ok(false))
            .with_rebase_onto_result(Ok(RebaseOutcome::Conflicted))
            .with_rebase_in_progress_sequence(vec![Ok(true), Ok(true), Ok(false)])
            .with_conflicted_files_sequence(vec![Ok(vec!["a.rs".to_string()])])
            .with_branch_exists_local(Ok(false));
        fx.tmux = fx
            .tmux
            .with_current_session_name(Ok(Some("dev".to_string())))
            .with_list_windows_sequence(vec![
                Ok(vec![shell_window()]),
                Ok(vec![shell_window(), merge_window()]),
            ]);
        fx
    }

    /// A [`Sleeper`] that, on every poll tick, records the status of every
    /// `merge-conflict` row (so a test can see the row while the session is
    /// open) and, like an adopting session would, writes a session marker
    /// pointing at the first one.
    struct RowProbe<'a> {
        store: &'a RunStore,
        sessions_dir: &'a Path,
        seen: std::cell::RefCell<Vec<Vec<crate::runs::RunStatus>>>,
    }

    impl Sleeper for RowProbe<'_> {
        fn sleep(&self, _secs: u64) {
            let rows: Vec<_> = self
                .store
                .list_runs()
                .unwrap()
                .into_iter()
                .filter(|run| run.kind == MERGE_CONFLICT_KIND)
                .collect();
            if let Some(row) = rows.first() {
                std::fs::create_dir_all(self.sessions_dir).unwrap();
                std::fs::write(self.sessions_dir.join("sess-merge"), row.id.to_string()).unwrap();
            }
            self.seen
                .borrow_mut()
                .push(rows.into_iter().map(|run| run.status).collect());
        }
    }

    fn conflict_rows(fx: &Fixture) -> Vec<crate::runs::Run> {
        fx.store
            .list_runs()
            .unwrap()
            .into_iter()
            .filter(|run| run.kind == MERGE_CONFLICT_KIND)
            .map(|run| fx.store.run_by_id(run.id).unwrap().unwrap())
            .collect()
    }

    fn window_env(fx: &Fixture) -> Vec<(String, String)> {
        fx.tmux
            .calls()
            .into_iter()
            .find_map(|c| match c {
                TmuxCall::NewWindowWithCommand { env, .. } => Some(env),
                TmuxCall::NewSessionWithCommand { env, .. } => Some(env),
                _ => None,
            })
            .expect("a conflict window should have been opened")
    }

    /// The single `merge-<stem>-*.prompt.md` file the conflict session wrote.
    fn conflict_prompt_file(fx: &Fixture, stem: &str) -> String {
        let prefix = format!("merge-{stem}-");
        let files: Vec<PathBuf> = std::fs::read_dir(&fx.state_dir)
            .expect("state dir should exist")
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(&prefix)
            })
            .collect();
        assert_eq!(files.len(), 1, "expected one prompt file, got {files:?}");
        std::fs::read_to_string(&files[0]).unwrap()
    }

    #[test]
    fn conflict_session_row_runs_while_open_and_ends_done_when_resolved() {
        let fx = resolving_conflict_fixture();
        std::fs::create_dir_all(&fx.sessions_dir).unwrap();
        std::fs::write(fx.sessions_dir.join("sess-other"), "999").unwrap();
        let probe = RowProbe {
            store: &fx.store,
            sessions_dir: &fx.sessions_dir,
            seen: Default::default(),
        };
        let mut deps = fx.deps();
        deps.sleeper = &probe;

        let (result, _out) = run(&deps, "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)));
        let seen = probe.seen.borrow().clone();
        assert!(!seen.is_empty(), "the poll loop slept at least once");
        for tick in &seen {
            assert_eq!(tick, &vec![crate::runs::RunStatus::Running]);
        }
        let rows = conflict_rows(&fx);
        assert_eq!(rows.len(), 1, "{rows:?}");
        let row = &rows[0];
        assert_eq!(row.status, crate::runs::RunStatus::Done);
        assert_eq!(row.ticket, "PROJ-1");
        assert_eq!(row.lane, MERGE_CONFLICT_KIND);
        assert_eq!(row.scope, fx.identity.scope());
        assert_eq!(row.branch.as_deref(), Some("proj-1-fix"));
        assert_eq!(row.worktree, "/repo");
        assert_eq!(row.agent.as_deref(), Some("claude"));
        assert_eq!(row.repo.as_deref(), Some("repo"));
        assert!(
            !fx.sessions_dir.join("sess-merge").exists(),
            "merge removes the marker, so the SessionEnd hook finds nothing to overwrite"
        );
        assert!(fx.sessions_dir.join("sess-other").exists());
    }

    #[test]
    fn conflict_session_over_the_memory_budget_is_refused_before_its_row_and_window() {
        let fx = resolving_conflict_fixture();
        let mut deps = fx.deps();
        deps.admission = crate::work::admission::AdmissionDeps::with_budget(1);

        let (result, _out) = run(&deps, "PROJ-1");

        let err = result.unwrap_err();
        assert!(
            err.to_string()
                .starts_with("not launching: memory budget exceeded: repo/claude merge-conflict"),
            "{err}"
        );
        assert!(conflict_rows(&fx).is_empty());
        assert!(
            !fx.tmux.calls().iter().any(|c| matches!(
                c,
                TmuxCall::NewWindowWithCommand { .. } | TmuxCall::NewSessionWithCommand { .. }
            )),
            "no agent window opened"
        );
    }

    #[test]
    fn batch_conflict_session_over_the_memory_budget_is_refused_before_its_row_and_window() {
        let fx = batch_fixture(&[BatchTicket::Conflict { resolves: true }; 2]);
        let mut deps = fx.deps();
        deps.admission = crate::work::admission::AdmissionDeps::with_budget(1);

        let (result, _out) = run_batch(&deps, &keys(2));

        let rendered = match result {
            Ok(report) => format!("{:?}", report.failed),
            Err(err) => err.to_string(),
        };
        assert!(rendered.contains("memory budget exceeded"), "{rendered}");
        assert!(conflict_rows(&fx).is_empty());
        assert!(
            !fx.tmux.calls().iter().any(|c| matches!(
                c,
                TmuxCall::NewWindowWithCommand { .. } | TmuxCall::NewSessionWithCommand { .. }
            )),
            "no agent window opened"
        );
    }

    #[test]
    fn conflict_session_gets_the_session_run_id_and_a_registration_preamble() {
        let fx = resolving_conflict_fixture();

        let _ = run(&fx.deps(), "PROJ-1");

        let id = conflict_rows(&fx)[0].id;
        let env = window_env(&fx);
        assert!(
            env.contains(&("TSKMSTR_SESSION_RUN_ID".to_string(), id.to_string())),
            "{env:?}"
        );
        assert!(
            env.iter().all(|(key, _)| key != "TSKMSTR_RUN_ID"),
            "an interactive session must never get the headless run id: {env:?}"
        );
        let prompt = conflict_prompt_file(&fx, "PROJ-1");
        assert!(
            prompt.contains("tm runs register --kind merge-conflict PROJ-1"),
            "{prompt}"
        );
    }

    #[test]
    fn conflict_session_row_ends_failed_on_timeout() {
        let mut fx = conflicted_fixture();
        fx.git = fx
            .git
            .with_rebase_in_progress_sequence(vec![Ok(true)])
            .with_conflicted_files_sequence(vec![Ok(vec!["a.rs".to_string()])]);
        fx.tmux = fx.tmux.with_list_windows(Ok(vec![merge_window()]));
        fx.clock = FakeClock::advancing(1_000, 901);

        let (result, _out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::ConflictsHandedBack)));
        let rows = conflict_rows(&fx);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, crate::runs::RunStatus::Failed);
        assert!(rows[0].blocker.is_some());
    }

    #[test]
    fn conflict_session_without_a_store_is_untracked() {
        let fx = resolving_conflict_fixture();
        let mut deps = fx.deps();
        deps.run_store = None;

        let (result, _out) = run(&deps, "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)));
        assert!(conflict_rows(&fx).is_empty());
        assert!(window_env(&fx).is_empty());
        assert!(!conflict_prompt_file(&fx, "PROJ-1").contains("tm runs register"));
    }

    #[test]
    fn batch_conflict_session_records_one_row_for_every_conflicted_ticket() {
        let fx = batch_fixture(&[BatchTicket::Conflict { resolves: true }; 2]);

        let (result, _out) = run_batch(&fx.deps(), &keys(2));

        assert_eq!(result.unwrap().merged, keys(2));
        let rows = conflict_rows(&fx);
        assert_eq!(rows.len(), 1, "one shared session, one row: {rows:?}");
        let row = &rows[0];
        assert_eq!(row.ticket, "PROJ-1");
        assert_eq!(row.branch, None);
        assert_eq!(row.status, crate::runs::RunStatus::Done);
        assert_eq!(row.agent.as_deref(), Some("claude"));
        assert_eq!(row.repo.as_deref(), Some("repo"));
        let events = fx.store.events_for_run(row.id).unwrap();
        let batch = events
            .iter()
            .find(|event| event.kind == "merge-batch")
            .expect("a merge-batch event");
        let detail: serde_json::Value =
            serde_json::from_str(batch.detail.as_deref().unwrap()).unwrap();
        assert_eq!(detail["tickets"], serde_json::json!(["PROJ-1", "PROJ-2"]));
        assert!(
            window_env(&fx).contains(&("TSKMSTR_SESSION_RUN_ID".to_string(), row.id.to_string()))
        );
        assert!(
            batch_prompt(&fx).contains("tm runs register --kind merge-conflict PROJ-1"),
            "the shared session adopts under the first key"
        );
    }

    #[test]
    fn batch_conflict_row_ends_failed_when_a_ticket_is_handed_back() {
        let mut fx = batch_fixture(&[
            BatchTicket::Conflict { resolves: true },
            BatchTicket::Conflict { resolves: false },
        ]);
        fx.clock = FakeClock::advancing(1_000, 300);

        let _ = run_batch(&fx.deps(), &keys(2));

        let rows = conflict_rows(&fx);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, crate::runs::RunStatus::Failed);
        assert!(
            rows[0]
                .blocker
                .as_deref()
                .is_some_and(|b| b.contains("PROJ-2")),
            "{:?}",
            rows[0].blocker
        );
    }

    // --- stale-base rejection: rebase and retry once (GitHub issue #109) ---

    fn stale_base_error() -> GhError {
        GhError::Command {
            command: "gh pr merge".to_string(),
            exit_code: Some(1),
            stderr: "X Pull request o/r#7 is not mergeable: the head branch is not up to date with the base branch.".to_string(),
        }
    }

    /// A checkout of `proj-1-fix` at the repo root that needs no rebase at
    /// prepare time, so any rebase recorded came from the merge retry.
    fn up_to_date_fixture() -> Fixture {
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
            .with_is_ancestor_result(Ok(true))
            .with_rebase_onto_result(Ok(RebaseOutcome::Completed))
            .with_branch_exists_local(Ok(false));
        fx
    }

    #[test]
    fn stale_base_rejection_rebases_pushes_and_retries_the_merge_once() {
        let mut fx = up_to_date_fixture();
        fx.gh = fx
            .gh
            .with_pr_merge_sequence(vec![Err(stale_base_error()), Ok(())]);

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)), "{out}");
        assert_eq!(
            fx.git.rebase_onto_calls(),
            vec![(PathBuf::from("/repo"), "origin/main".to_string())]
        );
        assert_eq!(fx.git.push_force_with_lease_calls().len(), 1);
        assert_eq!(fx.gh.pr_merge_calls().len(), 2);
        assert_eq!(
            &fx.git.call_log()[..4],
            &[
                "fetch_origin",
                "fetch_origin",
                "rebase_onto",
                "push_force_with_lease"
            ],
            "the retry fetches, rebases, then pushes before merging again"
        );
        assert!(out.contains("rebasing and retrying"), "{out}");
    }

    #[test]
    fn stale_base_rejection_twice_fails_after_a_single_retry() {
        let mut fx = up_to_date_fixture();
        fx.gh = fx.gh.with_pr_merge_result(Err(stale_base_error()));

        let (result, _out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Err(MergeError::Gh(_))));
        assert_eq!(fx.gh.pr_merge_calls().len(), 2, "exactly one retry");
    }

    #[test]
    fn other_merge_rejections_are_not_retried() {
        let mut fx = up_to_date_fixture();
        fx.gh = fx.gh.with_pr_merge_result(Err(GhError::Command {
            command: "gh pr merge".to_string(),
            exit_code: Some(1),
            stderr:
                "X Pull request o/r#7 is not mergeable: the base branch policy prohibits the merge."
                    .to_string(),
        }));

        let (result, _out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Err(MergeError::Gh(_))));
        assert_eq!(fx.gh.pr_merge_calls().len(), 1);
        assert!(fx.git.rebase_onto_calls().is_empty());
    }

    #[test]
    fn stale_base_rejection_without_a_checkout_fails_without_retrying() {
        let mut fx = Fixture::new();
        fx.gh = fx
            .gh
            .with_pr_list(Ok(vec![pr_info(7, "proj-1-fix", "main", "PROJ-1")]))
            .with_pr_merge_result(Err(stale_base_error()));

        let (result, _out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Err(MergeError::Gh(_))));
        assert_eq!(fx.gh.pr_merge_calls().len(), 1);
    }

    #[test]
    fn stale_base_retry_conflict_goes_through_the_conflict_session() {
        let mut fx = up_to_date_fixture();
        fx.gh = fx
            .gh
            .with_pr_merge_sequence(vec![Err(stale_base_error()), Ok(())]);
        fx.git = fx
            .git
            // "aaa" at prepare and before the retry rebase, "bbb" once the
            // conflict session completes it.
            .with_rev_parse_sequence(
                "proj-1-fix",
                vec![
                    Ok("aaa".to_string()),
                    Ok("aaa".to_string()),
                    Ok("bbb".to_string()),
                ],
            )
            .with_rebase_onto_result(Ok(RebaseOutcome::Conflicted))
            .with_rebase_in_progress_sequence(vec![Ok(false)]);

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)), "{out}");
        assert!(
            fx.tmux
                .calls()
                .iter()
                .any(|c| matches!(c, TmuxCall::NewSessionWithCommand { .. })),
            "the retry's conflicts should open a conflict session"
        );
        assert!(out.contains("conflicts resolved; rebase complete"), "{out}");
        assert_eq!(fx.git.push_force_with_lease_calls().len(), 1);
        assert_eq!(fx.gh.pr_merge_calls().len(), 2);
    }

    #[test]
    fn stale_base_retry_conflict_left_unresolved_hands_back() {
        let mut fx = up_to_date_fixture();
        fx.gh = fx
            .gh
            .with_pr_merge_sequence(vec![Err(stale_base_error()), Ok(())]);
        fx.git = fx
            .git
            .with_rebase_onto_result(Ok(RebaseOutcome::Conflicted))
            .with_rebase_in_progress_sequence(vec![Ok(true)])
            .with_conflicted_files_sequence(vec![Ok(vec!["a.rs".to_string()])]);
        fx.tmux = fx
            .tmux
            .with_current_session_name(Ok(Some("dev".to_string())))
            .with_list_windows(Ok(vec![]));

        let (result, _out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::ConflictsHandedBack)));
        assert!(fx.git.push_force_with_lease_calls().is_empty());
        assert_eq!(fx.gh.pr_merge_calls().len(), 1, "no retry until resolved");
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
            .with_issue("PROJ-1", issue_with_status("PROJ-1", "In Review"))
            .with_transitions(
                "PROJ-1",
                vec![crate::ticketing::types::Transition {
                    id: "31".to_string(),
                    name: "Ship it".to_string(),
                    to: Status {
                        name: "Done".to_string(),
                        status_category: StatusCategory {
                            key: "done".to_string(),
                        },
                    },
                }],
            );

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)));
        assert!(out.contains("moved PROJ-1 to Done"), "{out}");
    }

    #[test]
    fn status_on_merge_already_in_target_prints_a_no_op_not_a_warning() {
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
        assert!(out.contains("PROJ-1 already in Done"), "{out}");
        assert!(!out.contains("no transition"), "{out}");
    }

    // --- stale status-label sweep (GitHub issue #75) ---

    fn merged_fixture() -> Fixture {
        let mut fx = Fixture::new();
        fx.gh = fx
            .gh
            .with_pr_list(Ok(vec![pr_info(7, "proj-1-fix", "main", "PROJ-1")]));
        fx.git = fx
            .git
            .with_current_branch(Ok("main".to_string()))
            .with_branch_exists_local(Ok(false));
        fx
    }

    #[test]
    fn merge_without_status_on_merge_still_clears_stale_status_labels() {
        // The PR's closing keyword closes the issue outside tm; with no
        // status_on_merge nothing else would remove its tm:status/* label.
        let mut fx = merged_fixture();
        fx.jira = fx
            .jira
            .with_cleared_status_labels("PROJ-1", &["tm:status/in-review"]);

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)));
        assert_eq!(
            fx.jira.clear_closed_status_labels_calls(),
            vec!["PROJ-1".to_string()]
        );
        assert!(
            out.contains("cleared stale status label tm:status/in-review from PROJ-1"),
            "{out}"
        );
    }

    #[test]
    fn merge_with_nothing_to_clear_prints_nothing_about_labels() {
        let fx = merged_fixture();

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)));
        assert_eq!(fx.jira.clear_closed_status_labels_calls().len(), 1);
        assert!(!out.contains("status label"), "{out}");
    }

    #[test]
    fn stale_status_label_sweep_failure_only_warns() {
        let mut fx = merged_fixture();
        fx.jira = fx
            .jira
            .with_clear_closed_status_labels_error("PROJ-1", 0, "boom");

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)));
        assert!(
            out.contains("warning: failed to clear stale status labels from PROJ-1"),
            "{out}"
        );
    }

    // --- stacked children merged outside tm merge (GitHub issue #94) ---

    fn done_transition() -> crate::ticketing::types::Transition {
        crate::ticketing::types::Transition {
            id: "31".to_string(),
            name: "Ship it".to_string(),
            to: Status {
                name: "Done".to_string(),
                status_category: StatusCategory {
                    key: "done".to_string(),
                },
            },
        }
    }

    /// `merged_fixture` plus `PROJ-2`'s PR #8, already merged into
    /// `PROJ-1`'s branch, its ticket still In Review.
    fn stacked_fixture() -> Fixture {
        let mut fx = merged_fixture();
        fx.gh = fx.gh.with_pr_list_merged_into(
            "proj-1-fix",
            Ok(vec![pr_info(8, "proj-2-fix", "proj-1-fix", "PROJ-2")]),
        );
        fx.jira = fx
            .jira
            .with_issue("PROJ-2", issue_with_status("PROJ-2", "In Review"))
            .with_transitions("PROJ-2", vec![done_transition()]);
        fx
    }

    #[test]
    fn merge_moves_a_stacked_child_already_merged_into_the_branch() {
        let mut fx = stacked_fixture();
        fx.status_on_merge = Some("Done".to_string());

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)));
        assert_eq!(
            fx.gh.pr_list_merged_into_calls()[0],
            (PathBuf::from("/repo"), "proj-1-fix".to_string())
        );
        assert!(
            out.contains("moved stacked PROJ-2 (PR #8) to Done"),
            "{out}"
        );
        assert!(
            fx.jira
                .clear_closed_status_labels_calls()
                .contains(&"PROJ-2".to_string())
        );
    }

    #[test]
    fn stacked_child_falls_back_to_done_without_status_on_merge() {
        // Same target `tm drift --fix` suggests for MergedNotDone: the
        // child's closing keyword never fired, so nothing else closes it.
        let fx = stacked_fixture();

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)));
        assert!(
            out.contains("moved stacked PROJ-2 (PR #8) to Done"),
            "{out}"
        );
    }

    #[test]
    fn stacked_children_are_followed_transitively() {
        let mut fx = stacked_fixture();
        fx.gh = fx.gh.with_pr_list_merged_into(
            "proj-2-fix",
            Ok(vec![pr_info(9, "proj-3-fix", "proj-2-fix", "PROJ-3")]),
        );
        fx.jira = fx
            .jira
            .with_issue("PROJ-3", issue_with_status("PROJ-3", "Done"));

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)));
        assert!(
            out.contains("moved stacked PROJ-2 (PR #8) to Done"),
            "{out}"
        );
        assert!(
            out.contains("stacked PROJ-3 (PR #9) already in Done"),
            "{out}"
        );
    }

    #[test]
    fn stacked_child_lookup_failure_only_warns() {
        let mut fx = merged_fixture();
        fx.gh = fx.gh.with_pr_list_merged_into(
            "proj-1-fix",
            Err(GhError::Command {
                command: "gh pr list".to_string(),
                exit_code: Some(1),
                stderr: "boom".to_string(),
            }),
        );

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)));
        assert!(
            out.contains("warning: failed to list PRs merged into proj-1-fix"),
            "{out}"
        );
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
        // The tracked session's registration preamble comes first (GitHub
        // issue #156), then the substituted prompt_file contents.
        assert!(
            written.ends_with("\n\ncustom PROJ-1 proj-1-fix main prompt"),
            "expected the prompt_file contents (substituted) written to the prompt file: {written}"
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

    /// GitHub issue #143: with `[work.merge] model` unset, the conflict
    /// session falls back to `[work] default_model`, and with neither set,
    /// to the runner's built-in default.
    #[test]
    fn conflict_session_falls_back_to_the_work_default_then_the_builtin_model() {
        let fx = conflicted_fixture();
        let _ = run(&fx.deps(), "PROJ-1");
        let command = launched_command(&fx);
        assert!(command.contains("'--model' 'opus'"), "{command}");

        let mut fx = conflicted_fixture();
        fx.work_default_model = Some("sonnet".to_string());
        let _ = run(&fx.deps(), "PROJ-1");
        let command = launched_command(&fx);
        assert!(command.contains("'--model' 'sonnet'"), "{command}");

        let mut fx = conflicted_fixture();
        fx.work_default_model = Some("sonnet".to_string());
        fx.merge_cfg.model = Some("fable".to_string());
        let _ = run(&fx.deps(), "PROJ-1");
        let command = launched_command(&fx);
        assert!(command.contains("'--model' 'fable'"), "{command}");
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

    // --- run_merge_batch ---

    /// How one batch-test ticket's rebase behaves.
    #[derive(Debug, Clone, Copy)]
    enum BatchTicket {
        /// The rebase completes without conflicts.
        Clean,
        /// The rebase stops on conflicts; `resolves` says whether the
        /// shared session ever finishes it.
        Conflict { resolves: bool },
    }

    fn shell_window() -> TmuxWindow {
        TmuxWindow {
            session: "dev".to_string(),
            name: "shell".to_string(),
            dead: false,
        }
    }

    fn merge_window() -> TmuxWindow {
        TmuxWindow {
            session: "dev".to_string(),
            name: "merge".to_string(),
            dead: false,
        }
    }

    /// Sets up `PROJ-<n>` (1-based) for each entry of `tickets`: an open PR
    /// `#1<n>` from `proj-<n>-fix` onto `main`, a registered checkout, a
    /// base that moved (so a rebase runs), and the entry's rebase
    /// behavior. Runs inside tmux session `dev`.
    fn batch_fixture(tickets: &[BatchTicket]) -> Fixture {
        let mut fx = Fixture::new();
        let prs = (1..=tickets.len())
            .map(|n| {
                pr_info(
                    10 + n as u64,
                    &format!("proj-{n}-fix"),
                    "main",
                    &format!("PROJ-{n}"),
                )
            })
            .collect();
        fx.gh = fx.gh.with_pr_list(Ok(prs));
        for (i, ticket) in tickets.iter().enumerate() {
            let n = i + 1;
            let branch = format!("proj-{n}-fix");
            let dir = fx.register_checkout_for(&format!("PROJ-{n}"), &branch);
            let old_tip = format!("old-{n}");
            let git = std::mem::take(&mut fx.git)
                .with_rev_parse_result(format!("origin/{branch}"), Ok(old_tip.clone()));
            fx.git = match ticket {
                BatchTicket::Clean => git
                    .with_rev_parse_result(branch.clone(), Ok(old_tip))
                    .with_rebase_onto_result_for(dir, Ok(RebaseOutcome::Completed)),
                BatchTicket::Conflict { resolves } => git
                    .with_rev_parse_sequence(
                        branch.clone(),
                        vec![Ok(old_tip), Ok(format!("new-{n}"))],
                    )
                    .with_rebase_onto_result_for(dir.clone(), Ok(RebaseOutcome::Conflicted))
                    .with_rebase_in_progress_sequence_for(
                        dir,
                        if *resolves {
                            vec![Ok(true), Ok(false)]
                        } else {
                            vec![Ok(true)]
                        },
                    ),
            };
        }
        fx.git = std::mem::take(&mut fx.git)
            .with_current_branch(Ok("main".to_string()))
            .with_is_ancestor_result(Ok(false))
            .with_branch_exists_local(Ok(false))
            .with_conflicted_files_sequence(vec![Ok(vec!["a.rs".to_string()])]);
        fx.tmux = fx
            .tmux
            .with_current_session_name(Ok(Some("dev".to_string())))
            .with_list_windows_sequence(vec![
                Ok(vec![shell_window()]),
                Ok(vec![shell_window(), merge_window()]),
            ]);
        fx
    }

    fn keys(n: usize) -> Vec<String> {
        (1..=n).map(|n| format!("PROJ-{n}")).collect()
    }

    fn run_batch(
        deps: &MergeDeps<'_>,
        keys: &[String],
    ) -> (Result<BatchMergeReport, MergeError>, String) {
        let mut out = Vec::new();
        let result = run_merge_batch(deps, keys, &mut out);
        (result, String::from_utf8(out).unwrap())
    }

    fn opened_windows(fx: &Fixture) -> usize {
        fx.tmux
            .calls()
            .iter()
            .filter(|c| {
                matches!(
                    c,
                    TmuxCall::NewWindowWithCommand { .. } | TmuxCall::NewSessionWithCommand { .. }
                )
            })
            .count()
    }

    /// The single `merge-batch-*.prompt.md` file the batch session wrote.
    fn batch_prompt(fx: &Fixture) -> String {
        let files: Vec<PathBuf> = std::fs::read_dir(&fx.state_dir)
            .expect("state dir should exist")
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("merge-batch-")
            })
            .collect();
        assert_eq!(
            files.len(),
            1,
            "expected one batch prompt file, got {files:?}"
        );
        std::fs::read_to_string(&files[0]).unwrap()
    }

    #[test]
    fn batch_all_clean_merges_every_ticket_and_applies_each_status() {
        let mut fx = batch_fixture(&[BatchTicket::Clean; 3]);
        fx.status_on_merge = Some("Done".to_string());
        fx.jira = fx
            .jira
            .with_issue("PROJ-1", issue_with_status("PROJ-1", "Done"))
            .with_issue("PROJ-2", issue_with_status("PROJ-2", "Done"))
            .with_issue("PROJ-3", issue_with_status("PROJ-3", "Done"));

        let (result, out) = run_batch(&fx.deps(), &keys(3));

        let report = result.expect("batch should succeed");
        assert_eq!(report.merged, keys(3));
        assert!(report.handed_back.is_empty());
        assert!(report.failed.is_empty());
        let merged: Vec<u64> = fx.gh.pr_merge_calls().iter().map(|(_, n)| *n).collect();
        assert_eq!(merged, vec![11, 12, 13]);
        assert_eq!(fx.git.push_force_with_lease_calls().len(), 3);
        for key in keys(3) {
            assert!(out.contains(&format!("{key} already in Done")), "{out}");
        }
        assert_eq!(opened_windows(&fx), 0, "no conflict session needed");
    }

    #[test]
    fn batch_mixed_merges_clean_tickets_before_opening_one_shared_session() {
        let fx = batch_fixture(&[
            BatchTicket::Conflict { resolves: true },
            BatchTicket::Clean,
            BatchTicket::Conflict { resolves: true },
        ]);

        let (result, out) = run_batch(&fx.deps(), &keys(3));

        let report = result.expect("batch should succeed");
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert!(report.handed_back.is_empty());
        assert_eq!(report.merged, vec!["PROJ-2", "PROJ-1", "PROJ-3"]);
        // The clean ticket merged first, without waiting on the session.
        let merged: Vec<u64> = fx.gh.pr_merge_calls().iter().map(|(_, n)| *n).collect();
        assert_eq!(merged, vec![12, 11, 13]);
        let clean_merged_at = out.find("merged PR #12").expect("PROJ-2 merged");
        let session_at = out.find("opening agent session").expect("session opened");
        assert!(clean_merged_at < session_at, "{out}");
        assert_eq!(opened_windows(&fx), 1, "exactly one shared session");
        assert_eq!(fx.git.push_force_with_lease_calls().len(), 3);
    }

    #[test]
    fn batch_all_conflict_opens_one_session_naming_every_ticket() {
        let fx = batch_fixture(&[BatchTicket::Conflict { resolves: true }; 2]);

        let (result, _out) = run_batch(&fx.deps(), &keys(2));

        let report = result.expect("batch should succeed");
        assert_eq!(report.merged, keys(2));
        assert_eq!(opened_windows(&fx), 1, "no second window");
        let prompt = batch_prompt(&fx);
        for (key, branch) in [("PROJ-1", "proj-1-fix"), ("PROJ-2", "proj-2-fix")] {
            assert!(prompt.contains(key), "{prompt}");
            assert!(prompt.contains(branch), "{prompt}");
        }
        assert!(prompt.contains("main"), "{prompt}");
        assert!(!prompt.contains("{ticket_list}"), "{prompt}");
    }

    /// GitHub issue #143: the batch's shared conflict session resolves its
    /// model through the same chain as a single merge's.
    #[test]
    fn batch_conflict_session_passes_the_work_default_model() {
        let mut fx = batch_fixture(&[BatchTicket::Conflict { resolves: true }; 2]);
        fx.work_default_model = Some("sonnet".to_string());

        let _ = run_batch(&fx.deps(), &keys(2));

        let command = launched_command(&fx);
        assert!(command.contains("'--model' 'sonnet'"), "{command}");
    }

    #[test]
    fn batch_partial_resolution_merges_resolved_and_hands_back_the_rest() {
        let mut fx = batch_fixture(&[
            BatchTicket::Conflict { resolves: true },
            BatchTicket::Conflict { resolves: false },
        ]);
        fx.clock = FakeClock::advancing(1_000, 300);

        let (result, out) = run_batch(&fx.deps(), &keys(2));

        let report = result.expect("batch should succeed");
        assert_eq!(report.merged, vec!["PROJ-1"]);
        assert_eq!(report.handed_back, vec!["PROJ-2"]);
        assert!(report.failed.is_empty());
        let merged: Vec<u64> = fx.gh.pr_merge_calls().iter().map(|(_, n)| *n).collect();
        assert_eq!(merged, vec![11]);
        assert!(out.contains("timed out"), "{out}");
        assert!(out.contains("rebase --abort"), "{out}");
    }

    #[test]
    fn batch_conflict_prompt_overrides_the_default_template() {
        let mut fx = batch_fixture(&[BatchTicket::Conflict { resolves: true }; 2]);
        fx.merge_cfg.batch_conflict_prompt = Some("CUSTOM BATCH\n{ticket_list}".to_string());

        let (result, _out) = run_batch(&fx.deps(), &keys(2));

        result.expect("batch should succeed");
        let prompt = batch_prompt(&fx);
        assert!(prompt.contains("\n\nCUSTOM BATCH\n"), "{prompt}");
        assert!(
            prompt.contains("PROJ-1") && prompt.contains("PROJ-2"),
            "{prompt}"
        );
    }

    #[test]
    fn batch_conflict_prompt_substitutes_upstream_context_per_ticket() {
        let mut fx = batch_fixture(&[BatchTicket::Conflict { resolves: true }; 2]);
        fx.merge_cfg.batch_conflict_prompt =
            Some("CUSTOM BATCH\n{ticket_list}\n{upstream_context}".to_string());
        fx.git =
            std::mem::take(&mut fx.git).with_commits_touching_result(Ok(vec!["s1".to_string()]));
        fx.gh = std::mem::take(&mut fx.gh)
            .with_commit_pulls("s1", Ok(vec![commit_pull(12, "Split config", "Why")]));

        let (result, _out) = run_batch(&fx.deps(), &keys(2));

        result.expect("batch should succeed");
        let prompt = batch_prompt(&fx);
        assert!(!prompt.contains("{upstream_context}"), "{prompt}");
        assert!(prompt.contains("PROJ-1 upstream context:\n"), "{prompt}");
        assert!(prompt.contains("PROJ-2 upstream context:\n"), "{prompt}");
        assert_eq!(
            prompt.matches("PR #12: Split config").count(),
            2,
            "{prompt}"
        );
        let calls = fx.git.commits_touching_calls();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!(calls[0].0, "old-1");
        assert_eq!(calls[1].0, "old-2");
    }

    #[test]
    fn batch_single_conflict_uses_the_single_ticket_session() {
        let fx = batch_fixture(&[BatchTicket::Clean, BatchTicket::Conflict { resolves: true }]);

        let (result, _out) = run_batch(&fx.deps(), &keys(2));

        let report = result.expect("batch should succeed");
        assert_eq!(report.merged, vec!["PROJ-1", "PROJ-2"]);
        assert_eq!(opened_windows(&fx), 1);
        let names: Vec<String> = std::fs::read_dir(&fx.state_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().any(|n| n.starts_with("merge-PROJ-2-")),
            "expected the single-ticket prompt file, got {names:?}"
        );
    }

    #[test]
    fn batch_member_rejected_for_a_stale_base_is_rebased_and_retried() {
        let mut fx = batch_fixture(&[BatchTicket::Clean; 2]);
        // PROJ-1 merges, moving main under PROJ-2, whose first merge
        // attempt is then rejected.
        fx.gh = fx
            .gh
            .with_pr_merge_sequence(vec![Ok(()), Err(stale_base_error()), Ok(())]);

        let (result, out) = run_batch(&fx.deps(), &keys(2));

        let report = result.expect("batch should succeed");
        assert_eq!(report.merged, keys(2), "{out}");
        assert!(report.failed.is_empty(), "{out}");
        let merged: Vec<u64> = fx.gh.pr_merge_calls().iter().map(|(_, n)| *n).collect();
        assert_eq!(merged, vec![11, 12, 12]);
        assert_eq!(
            fx.git.rebase_onto_calls().len(),
            3,
            "one prepare rebase per ticket plus PROJ-2's retry"
        );
    }

    #[test]
    fn batch_member_still_stale_after_the_retry_is_reported_failed() {
        let mut fx = batch_fixture(&[BatchTicket::Clean; 2]);
        fx.gh = fx.gh.with_pr_merge_sequence(vec![
            Ok(()),
            Err(stale_base_error()),
            Err(stale_base_error()),
        ]);

        let (result, out) = run_batch(&fx.deps(), &keys(2));

        let report = result.expect("batch should succeed");
        assert_eq!(report.merged, vec!["PROJ-1"], "{out}");
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, "PROJ-2");
        assert_eq!(fx.gh.pr_merge_calls().len(), 3, "exactly one retry");
    }

    #[test]
    fn batch_ticket_error_is_recorded_and_the_rest_still_merge() {
        let fx = batch_fixture(&[BatchTicket::Clean; 2]);
        let keys = vec![
            "PROJ-1".to_string(),
            "PROJ-9".to_string(),
            "PROJ-2".to_string(),
        ];

        let (result, out) = run_batch(&fx.deps(), &keys);

        let report = result.expect("batch should succeed");
        assert_eq!(report.merged, vec!["PROJ-1", "PROJ-2"]);
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, "PROJ-9");
        assert!(report.failed[0].1.contains("no open pull request"));
        assert!(out.contains("PROJ-9"), "{out}");
    }

    #[test]
    fn batch_window_death_hands_back_every_unresolved_ticket() {
        let mut fx = batch_fixture(&[BatchTicket::Conflict { resolves: false }; 2]);
        fx.tmux = FakeTmuxOps::new()
            .with_current_session_name(Ok(Some("dev".to_string())))
            .with_list_windows_sequence(vec![Ok(vec![shell_window()])]);

        let (result, out) = run_batch(&fx.deps(), &keys(2));

        let report = result.expect("batch should succeed");
        assert!(report.merged.is_empty());
        assert_eq!(report.handed_back, keys(2));
        assert!(fx.gh.pr_merge_calls().is_empty());
        assert!(out.contains("conflict session ended"), "{out}");
    }

    // --- ticket session archive-then-kill (GitHub issue #78) ---

    fn ticket_window(key: &str, name: &str) -> TmuxWindow {
        TmuxWindow {
            session: format!("tm-proj-{}", key.to_lowercase()),
            name: name.to_string(),
            dead: false,
        }
    }

    /// A clean, no-rebase merge of `PROJ-1` whose ticket session
    /// `tm-proj-proj-1` is alive with `work` and `shell` windows.
    fn session_fixture() -> Fixture {
        let mut fx = merged_fixture();
        fx.tmux = FakeTmuxOps::new()
            .with_has_session(Ok(true))
            .with_list_windows(Ok(vec![
                ticket_window("PROJ-1", "work"),
                ticket_window("PROJ-1", "shell"),
            ]));
        fx
    }

    /// Marks every recorded run done, so no run counts as live in its
    /// session (fixtures register `running` checkout rows).
    fn finish_all_runs(fx: &Fixture) {
        for run in fx.store.all_runs().unwrap() {
            fx.store
                .finish_run(
                    run.id,
                    &crate::runs::FinishRun {
                        status: crate::runs::RunStatus::Done,
                        ..Default::default()
                    },
                )
                .unwrap();
        }
    }

    fn killed(fx: &Fixture, key: &str) -> bool {
        fx.tmux.calls().contains(&TmuxCall::KillSession(format!(
            "tm-proj-{}",
            key.to_lowercase()
        )))
    }

    #[test]
    fn merge_archives_each_window_of_the_ticket_session_before_killing_it() {
        let fx = session_fixture();

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)), "{out}");
        let gc_calls: Vec<TmuxCall> = fx
            .tmux
            .calls()
            .into_iter()
            .filter(|c| matches!(c, TmuxCall::CapturePane { .. } | TmuxCall::KillSession(_)))
            .collect();
        assert_eq!(
            gc_calls,
            vec![
                TmuxCall::CapturePane {
                    name: "tm-proj-proj-1".to_string(),
                    window: "work".to_string()
                },
                TmuxCall::CapturePane {
                    name: "tm-proj-proj-1".to_string(),
                    window: "shell".to_string()
                },
                TmuxCall::KillSession("tm-proj-proj-1".to_string()),
            ]
        );
        let rows = fx
            .store
            .session_archives_for_ticket(Some(&fx.identity.scope()), "PROJ-1")
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert!(
            Path::new(&rows[0].path).starts_with(fx.state_dir.join("archive")),
            "{rows:?}"
        );
        assert!(out.contains("killed tmux session tm-proj-proj-1"), "{out}");
    }

    #[test]
    fn merge_with_no_ticket_session_is_quiet_about_tmux() {
        let fx = merged_fixture();

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)));
        assert!(!out.contains("tmux session"), "{out}");
        assert!(!killed(&fx, "PROJ-1"));
    }

    #[test]
    fn merge_capture_failure_leaves_the_session_alive_and_still_merges() {
        let mut fx = session_fixture();
        fx.tmux = std::mem::take(&mut fx.tmux).with_capture_pane_failure(
            "shell",
            TmuxError::Command {
                command: "tmux capture-pane".to_string(),
                exit_code: Some(1),
                stderr: "boom".to_string(),
            },
        );

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)), "{out}");
        assert!(!killed(&fx, "PROJ-1"));
        assert!(out.contains("warning:"), "{out}");
        assert!(out.contains("tmux kill-session -t tm-proj-proj-1"), "{out}");
    }

    // --- finishing the merged ticket's live runs (GitHub issue #107) ---

    /// Starts a `kind` run for `key` in the fixture's scope with `pid`.
    fn start(fx: &Fixture, key: &str, kind: &str, pid: Option<u32>) -> i64 {
        fx.store
            .start_run(&StartRun {
                scope: fx.identity.scope(),
                ticket: key.to_string(),
                lane: kind.to_string(),
                worktree: "/repo".to_string(),
                branch: None,
                pid,
                kind: kind.to_string(),
                log_path: None,
            })
            .unwrap()
    }

    fn status_of(fx: &Fixture, id: i64) -> crate::runs::RunStatus {
        fx.store
            .all_runs()
            .unwrap()
            .into_iter()
            .find(|run| run.id == id)
            .unwrap()
            .status
    }

    #[test]
    fn merge_finishes_the_tickets_live_runs_and_stops_their_agents() {
        let fx = session_fixture();
        // The test process itself: a pid guaranteed to probe alive.
        let lane = start(&fx, "PROJ-1", "lane", Some(std::process::id()));
        let audit = start(&fx, "PROJ-1", "audit", None);

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)), "{out}");
        assert_eq!(status_of(&fx, lane), crate::runs::RunStatus::Done);
        assert_eq!(status_of(&fx, audit), crate::runs::RunStatus::Done);
        assert_eq!(*fx.killed_pids.borrow(), vec![std::process::id()]);
        assert!(out.contains(&format!("finished run {lane}")), "{out}");
        assert!(killed(&fx, "PROJ-1"), "{out}");
    }

    /// A watch that already finished never sees the merge itself, so `tm
    /// merge` records the `pr_closed` it would have (GitHub issue #119):
    /// otherwise the overview keeps showing the ticket as awaiting review.
    #[test]
    fn merge_records_pr_closed_on_the_tickets_latest_finished_watch() {
        let fx = session_fixture();
        let older = start(&fx, "PROJ-1", "review-watch", None);
        let watch = start(&fx, "PROJ-1", "review-watch", None);
        for id in [older, watch] {
            fx.store
                .finish_run(
                    id,
                    &crate::runs::FinishRun {
                        status: crate::runs::RunStatus::Done,
                        ..Default::default()
                    },
                )
                .unwrap();
        }

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)), "{out}");
        let closed = |id| {
            fx.store
                .events_for_run(id)
                .unwrap()
                .into_iter()
                .filter(|e| e.kind == "pr_closed")
                .map(|e| e.detail)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            closed(watch),
            vec![Some(r#"{"reason":"merged"}"#.to_string())]
        );
        assert!(closed(older).is_empty());
    }

    #[test]
    fn merge_does_not_repeat_a_pr_closed_the_watch_already_recorded() {
        let fx = session_fixture();
        let watch = start(&fx, "PROJ-1", "review-watch", None);
        fx.store
            .add_event(watch, "pr_closed", Some(r#"{"reason":"merged"}"#))
            .unwrap();

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)), "{out}");
        let events = fx.store.events_for_run(watch).unwrap();
        assert_eq!(events.iter().filter(|e| e.kind == "pr_closed").count(), 1);
    }

    #[test]
    fn a_hibernated_lane_of_the_merged_ticket_no_longer_blocks_the_kill() {
        let fx = session_fixture();
        let lane = start(&fx, "PROJ-1", "lane", Some(std::process::id()));
        assert!(fx.store.hibernate_run(lane).unwrap());

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)), "{out}");
        assert_eq!(status_of(&fx, lane), crate::runs::RunStatus::Done);
        // Hibernation already stopped its agent; the pid may since be reused.
        assert!(fx.killed_pids.borrow().is_empty());
        assert!(killed(&fx, "PROJ-1"), "{out}");
    }

    #[test]
    fn merge_leaves_a_session_hosting_another_tickets_live_run() {
        let fx = session_fixture();
        let other = start(&fx, "PROJ-2", "lane", None);
        fx.store
            .update_tmux_session(other, "tm-proj-proj-1")
            .unwrap();

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)), "{out}");
        assert_eq!(status_of(&fx, other), crate::runs::RunStatus::Running);
        assert!(!killed(&fx, "PROJ-1"));
        assert!(out.contains("is still running"), "{out}");
    }

    #[test]
    fn merge_inside_the_ticket_session_finishes_runs_without_stopping_agents() {
        // The agent hosting tm is likely the one that would be stopped.
        let mut fx = session_fixture();
        fx.tmux = std::mem::take(&mut fx.tmux)
            .with_current_session_name(Ok(Some("tm-proj-proj-1".to_string())));
        let lane = start(&fx, "PROJ-1", "lane", Some(std::process::id()));

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)), "{out}");
        assert_eq!(status_of(&fx, lane), crate::runs::RunStatus::Done);
        assert!(fx.killed_pids.borrow().is_empty());
        assert!(out.contains("left its agent (pid"), "{out}");
    }

    #[test]
    fn merge_closes_out_a_stacked_childs_runs_and_session() {
        let mut fx = stacked_fixture();
        fx.tmux = FakeTmuxOps::new().with_has_session(Ok(true));
        let child = start(&fx, "PROJ-2", "lane", None);

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)), "{out}");
        assert_eq!(status_of(&fx, child), crate::runs::RunStatus::Done);
        assert!(killed(&fx, "PROJ-2"), "{out}");
    }

    #[test]
    fn merge_never_kills_the_session_it_is_running_in() {
        let mut fx = session_fixture();
        fx.tmux = std::mem::take(&mut fx.tmux)
            .with_current_session_name(Ok(Some("tm-proj-proj-1".to_string())));

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)), "{out}");
        assert!(!killed(&fx, "PROJ-1"));
        assert!(out.contains("inside it"), "{out}");
    }

    #[test]
    fn batch_kills_every_merged_tickets_session_but_not_a_handed_back_one() {
        let mut fx = batch_fixture(&[
            BatchTicket::Clean,
            BatchTicket::Conflict { resolves: false },
        ]);
        fx.clock = FakeClock::advancing(1_000, 300);
        fx.tmux = std::mem::take(&mut fx.tmux).with_has_session(Ok(true));
        finish_all_runs(&fx);

        let (result, out) = run_batch(&fx.deps(), &keys(2));

        let report = result.expect("batch should succeed");
        assert_eq!(report.merged, vec!["PROJ-1"], "{out}");
        assert_eq!(report.handed_back, vec!["PROJ-2"], "{out}");
        assert!(killed(&fx, "PROJ-1"), "{out}");
        assert!(!killed(&fx, "PROJ-2"), "{out}");
    }

    #[test]
    fn batch_keeps_the_shared_conflict_session_while_a_ticket_is_handed_back() {
        // Outside tmux, the shared window opens in the first ticket's
        // session; killing it after PROJ-1 merges would take the window
        // PROJ-2 is handed back to with it.
        let mut fx = batch_fixture(&[
            BatchTicket::Conflict { resolves: true },
            BatchTicket::Conflict { resolves: false },
        ]);
        fx.clock = FakeClock::advancing(1_000, 300);
        fx.tmux = FakeTmuxOps::new()
            .with_has_session(Ok(true))
            .with_list_windows_sequence(vec![
                Ok(vec![]),
                Ok(vec![ticket_window("PROJ-1", "merge")]),
            ]);
        finish_all_runs(&fx);

        let (result, out) = run_batch(&fx.deps(), &keys(2));

        let report = result.expect("batch should succeed");
        assert_eq!(report.merged, vec!["PROJ-1"], "{out}");
        assert_eq!(report.handed_back, vec!["PROJ-2"], "{out}");
        assert!(!killed(&fx, "PROJ-1"), "{out}");
        assert!(
            out.contains("tm-proj-proj-1 running; it hosts the conflict session"),
            "{out}"
        );
    }

    #[test]
    fn batch_report_exit_code_prefers_failure_then_handback() {
        let mut report = BatchMergeReport::default();
        assert_eq!(report.exit_code(), 0);
        report.handed_back.push("PROJ-1".to_string());
        assert_eq!(report.exit_code(), 2);
        report
            .failed
            .push(("PROJ-2".to_string(), "boom".to_string()));
        assert_eq!(report.exit_code(), 1);
    }

    // --- branch memory consolidation (GitHub issue #208) ---

    /// Every window `tm merge` opened for the memory-consolidation session:
    /// `(session, dir, command)`.
    fn memory_windows(fx: &Fixture) -> Vec<(String, String, String)> {
        fx.tmux
            .calls()
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
                } if window_name.starts_with(MERGE_MEMORY_WINDOW_BASE) => {
                    Some((name, dir, command))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn successful_merge_starts_a_headless_memory_session_for_the_branch() {
        let mut fx = up_to_date_fixture();
        fx.merge_cfg.consolidate_memories = true;

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)), "{out}");
        let windows = memory_windows(&fx);
        assert_eq!(windows.len(), 1, "{windows:?}");
        let (session, dir, command) = &windows[0];
        assert_eq!(
            session,
            &ticket_session_name(&fx.identity.session_slug(), "PROJ-1"),
            "outside tmux the session gets its own ticket session"
        );
        assert_eq!(
            dir, "/repo",
            "runs in the repo root, not the removed worktree"
        );
        assert!(
            command.starts_with("exec env -u ANTHROPIC_API_KEY"),
            "{command}"
        );
        assert!(
            command.contains("'-p'"),
            "headless, not interactive: {command}"
        );
        assert!(
            command.contains("proj-1-fix"),
            "names the branch: {command}"
        );
        assert!(
            command.contains("> "),
            "output goes to a log file: {command}"
        );
        assert!(out.contains("consolidating thatch memories"), "{out}");
    }

    #[test]
    fn memory_session_is_confined_to_thatch_tools_with_a_small_turn_budget() {
        let mut fx = up_to_date_fixture();
        fx.merge_cfg.consolidate_memories = true;

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)), "{out}");
        let windows = memory_windows(&fx);
        let (_, _, command) = &windows[0];
        assert!(
            command.contains("'--allowedTools' 'mcp__thatch'"),
            "only thatch's MCP tools are allowed: {command}"
        );
        assert!(
            !command.contains("bypassPermissions"),
            "no bypass in the operator's own checkout: {command}"
        );
        assert!(
            command.contains(&format!("'--max-turns' '{MERGE_MEMORY_MAX_TURNS}'")),
            "{command}"
        );
    }

    #[test]
    fn memory_prompt_only_asks_for_thatch_calls_that_exist() {
        let mut fx = up_to_date_fixture();
        fx.merge_cfg.consolidate_memories = true;

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)), "{out}");
        let windows = memory_windows(&fx);
        let (_, _, command) = &windows[0];
        // memory_list takes only `store`; branch scope shows up as a
        // `(branch:<name>)` suffix on each label in its output.
        assert!(command.contains("memory_list once"), "{command}");
        assert!(command.contains("(branch:<name>)"), "{command}");
        assert!(command.contains("archived: true"), "{command}");
        assert!(command.contains("exact full label"), "{command}");
    }

    #[test]
    fn memory_session_is_skipped_when_disabled() {
        let fx = up_to_date_fixture();

        let (result, _out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)));
        assert!(memory_windows(&fx).is_empty());
    }

    #[test]
    fn failed_merge_starts_no_memory_session() {
        let mut fx = up_to_date_fixture();
        fx.merge_cfg.consolidate_memories = true;
        fx.gh = fx.gh.with_pr_merge_result(Err(GhError::Command {
            command: "gh pr merge".to_string(),
            exit_code: Some(1),
            stderr: "X not mergeable".to_string(),
        }));

        let (result, _out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Err(MergeError::Gh(_))));
        assert!(memory_windows(&fx).is_empty());
    }

    #[test]
    fn handed_back_merge_starts_no_memory_session() {
        let mut fx = Fixture::new();
        fx.merge_cfg.consolidate_memories = true;
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
            .with_list_windows(Ok(vec![merge_window()]));
        fx.clock = FakeClock::advancing(1_000, 901);

        let (result, _out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::ConflictsHandedBack)));
        assert!(memory_windows(&fx).is_empty());
    }

    #[test]
    fn memory_session_failure_only_warns_and_keeps_the_merge() {
        let mut fx = up_to_date_fixture();
        fx.merge_cfg.consolidate_memories = true;
        // A state_dir that is a file can't hold the session's log.
        let blocker = fx._state_dir.path().join("not-a-dir");
        std::fs::write(&blocker, "").unwrap();
        fx.state_dir = blocker;

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)), "{out}");
        assert!(memory_windows(&fx).is_empty());
        assert!(
            out.contains("warning: memory consolidation not started"),
            "{out}"
        );
    }

    #[test]
    fn batch_merge_starts_one_memory_session_naming_every_merged_branch() {
        let mut fx = batch_fixture(&[
            BatchTicket::Clean,
            BatchTicket::Conflict { resolves: false },
            BatchTicket::Clean,
        ]);
        fx.merge_cfg.consolidate_memories = true;
        fx.clock = FakeClock::advancing(1_000, 300);

        let (result, out) = run_batch(&fx.deps(), &keys(3));

        let report = result.expect("batch should succeed");
        assert_eq!(report.merged, vec!["PROJ-1", "PROJ-3"]);
        let windows = memory_windows(&fx);
        assert_eq!(windows.len(), 1, "one session for the batch: {windows:?}");
        let (session, _dir, command) = &windows[0];
        assert_eq!(session, "dev", "inside tmux it joins the current session");
        assert!(command.contains("proj-1-fix"), "{command}");
        assert!(command.contains("proj-3-fix"), "{command}");
        assert!(
            !command.contains("proj-2-fix"),
            "a handed-back branch is not merged: {command}"
        );
        assert!(out.contains("consolidating thatch memories"), "{out}");
    }

    // --- [work.merge] strategy (GitHub issue #244) ---

    use crate::config::MergeStrategy;
    use crate::github::gh_cli::RepoMergeMethod;

    /// A single ticket whose base moved, checked out at a registered
    /// worktree, under `strategy`. Runs inside tmux session `dev`.
    fn strategy_fixture(strategy: MergeStrategy) -> Fixture {
        let mut fx = Fixture::new();
        fx.merge_cfg.strategy = strategy;
        fx.register_checkout("proj-1-fix");
        fx.gh = fx
            .gh
            .with_pr_list(Ok(vec![pr_info(7, "proj-1-fix", "main", "PROJ-1")]));
        fx.git = fx
            .git
            .with_current_branch(Ok("main".to_string()))
            .with_rev_parse_result("origin/proj-1-fix", Ok("aaa".to_string()))
            .with_is_ancestor_result(Ok(false))
            .with_branch_exists_local(Ok(false));
        fx.tmux = fx
            .tmux
            .with_current_session_name(Ok(Some("dev".to_string())))
            .with_list_windows_sequence(vec![
                Ok(vec![shell_window()]),
                Ok(vec![shell_window(), merge_window()]),
            ]);
        fx
    }

    /// Puts the registered checkout back on its branch once a conflict
    /// session finishes, as a real resolution leaves it.
    fn on_branch_after_resolution(fx: &mut Fixture) {
        fx.git = std::mem::take(&mut fx.git).with_current_branch(Ok("proj-1-fix".to_string()));
    }

    fn single_prompt(fx: &Fixture) -> String {
        std::fs::read_to_string(fx.state_dir.join("merge-PROJ-1-1000.prompt.md"))
            .expect("prompt file should exist")
    }

    #[test]
    fn squash_strategy_squashes_since_the_merge_base_before_rebasing() {
        let mut fx = strategy_fixture(MergeStrategy::Squash);
        fx.git = fx
            .git
            .with_rev_parse_sequence(
                "proj-1-fix",
                vec![Ok("aaa".to_string()), Ok("squashed".to_string())],
            )
            .with_merge_base_result(Ok("base-sha".to_string()))
            .with_rebase_onto_result(Ok(RebaseOutcome::Completed));

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)), "{out}");
        assert_eq!(
            fx.git.call_log(),
            vec![
                "fetch_origin",
                "squash_since",
                "rebase_onto",
                "push_force_with_lease",
                "fetch_origin",
                "merge_ff_only",
            ]
        );
        let squash = fx.git.squash_since_calls();
        assert_eq!(squash.len(), 1);
        assert_eq!(squash[0].1, "base-sha");
        assert_eq!(squash[0].2, "[PROJ-1] fix the thing");
        assert!(out.contains("squashing proj-1-fix"), "{out}");
        assert!(!out.contains("warning"), "{out}");
    }

    #[test]
    fn squash_strategy_skips_the_squash_when_no_rebase_is_needed() {
        let mut fx = up_to_date_fixture();
        fx.merge_cfg.strategy = MergeStrategy::Squash;

        let (result, _out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)));
        assert!(fx.git.squash_since_calls().is_empty());
    }

    #[test]
    fn squash_strategy_warns_when_the_repo_merges_with_a_merge_commit() {
        let mut fx = strategy_fixture(MergeStrategy::Squash);
        fx.gh = fx
            .gh
            .with_repo_merge_method_result(Ok(Some(RepoMergeMethod::MergeCommit)));
        fx.git = fx
            .git
            .with_rev_parse_sequence(
                "proj-1-fix",
                vec![Ok("aaa".to_string()), Ok("squashed".to_string())],
            )
            .with_rebase_onto_result(Ok(RebaseOutcome::Completed));

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)), "{out}");
        assert!(
            out.contains("warning: this repo merges PRs with a merge commit"),
            "{out}"
        );
        assert_eq!(fx.git.squash_since_calls().len(), 1, "warn, never refuse");
    }

    #[test]
    fn squash_strategy_conflict_resolves_in_one_session_against_the_squashed_tip() {
        let mut fx = strategy_fixture(MergeStrategy::Squash);
        fx.git = std::mem::take(&mut fx.git)
            // "aaa" before the squash, "squashed" right after it (the tip
            // an abort would restore), "bbb" once the rebase completes.
            .with_rev_parse_sequence(
                "proj-1-fix",
                vec![
                    Ok("aaa".to_string()),
                    Ok("squashed".to_string()),
                    Ok("bbb".to_string()),
                ],
            )
            .with_rebase_onto_result(Ok(RebaseOutcome::Conflicted))
            .with_rebase_in_progress_sequence(vec![Ok(true), Ok(false)]);
        on_branch_after_resolution(&mut fx);

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)), "{out}");
        assert_eq!(opened_windows(&fx), 1);
        assert!(single_prompt(&fx).contains("git rebase --continue"));
        assert_eq!(fx.git.push_force_with_lease_calls().len(), 1);
    }

    #[test]
    fn squash_strategy_abort_names_the_unsquashed_tip_to_restore() {
        let mut fx = strategy_fixture(MergeStrategy::Squash);
        fx.git = fx
            .git
            .with_rev_parse_sequence(
                "proj-1-fix",
                vec![Ok("aaa".to_string()), Ok("squashed".to_string())],
            )
            .with_rebase_onto_result(Ok(RebaseOutcome::Conflicted))
            .with_rebase_in_progress_sequence(vec![Ok(false)]);
        on_branch_after_resolution(&mut fx);

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::ConflictsHandedBack)));
        assert!(out.contains("rebase was aborted"), "{out}");
        assert!(out.contains("reset --hard aaa"), "{out}");
    }

    #[test]
    fn squash_strategy_logs_the_unsquashed_tip_before_a_later_error() {
        let mut fx = strategy_fixture(MergeStrategy::Squash);
        fx.git = fx
            .git
            .with_rev_parse_sequence(
                "proj-1-fix",
                vec![Ok("aaa".to_string()), Ok("squashed".to_string())],
            )
            .with_rebase_onto_result(Err(GitError::Command {
                command: "git rebase".to_string(),
                exit_code: Some(128),
                stderr: "fatal: something broke".to_string(),
            }));

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(result.is_err(), "{out}");
        assert!(out.contains("squashed proj-1-fix (was aaa)"), "{out}");
    }

    #[test]
    fn merge_strategy_merges_the_base_in_and_pushes_without_force() {
        let mut fx = strategy_fixture(MergeStrategy::Merge);
        fx.git = fx
            .git
            .with_rev_parse_result("proj-1-fix", Ok("aaa".to_string()))
            .with_merge_into_result(Ok(RebaseOutcome::Completed));

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)), "{out}");
        assert_eq!(
            fx.git.call_log(),
            vec![
                "fetch_origin",
                "merge_into",
                "push",
                "fetch_origin",
                "merge_ff_only",
            ]
        );
        assert_eq!(
            fx.git.merge_into_calls()[0].1,
            "origin/main",
            "merges the base into the branch"
        );
        assert!(fx.git.rebase_onto_calls().is_empty());
        assert!(fx.git.push_force_with_lease_calls().is_empty());
        assert!(out.contains("merging origin/main into proj-1-fix"), "{out}");
    }

    #[test]
    fn merge_strategy_conflict_polls_the_merge_and_prompts_for_git_commit() {
        let mut fx = strategy_fixture(MergeStrategy::Merge);
        fx.git = fx
            .git
            .with_rev_parse_sequence(
                "proj-1-fix",
                vec![Ok("aaa".to_string()), Ok("merged".to_string())],
            )
            .with_merge_into_result(Ok(RebaseOutcome::Conflicted))
            .with_merge_in_progress_sequence(vec![Ok(true), Ok(false)])
            // A stale rebase marker must not be read as the merge's state.
            .with_rebase_in_progress_sequence(vec![Ok(true)]);
        on_branch_after_resolution(&mut fx);

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)), "{out}");
        assert!(out.contains("conflicts resolved; merge complete"), "{out}");
        let prompt = single_prompt(&fx);
        assert!(prompt.contains("git commit --no-edit"), "{prompt}");
        assert!(!prompt.contains("rebase --continue"), "{prompt}");
        assert_eq!(fx.git.push_calls().len(), 1);
        assert!(fx.git.push_force_with_lease_calls().is_empty());
    }

    #[test]
    fn merge_strategy_abort_hands_back_with_a_merge_abort_hint_on_timeout() {
        let mut fx = strategy_fixture(MergeStrategy::Merge);
        fx.git = fx
            .git
            .with_rev_parse_result("proj-1-fix", Ok("aaa".to_string()))
            .with_merge_into_result(Ok(RebaseOutcome::Conflicted))
            .with_merge_in_progress_sequence(vec![Ok(true)]);
        // Every clock read jumps past the conflict timeout.
        fx.clock = FakeClock::advancing(1_000, CONFLICT_TIMEOUT_SECS + 1);

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::ConflictsHandedBack)));
        assert!(out.contains("merge --abort"), "{out}");
        assert!(fx.git.push_calls().is_empty());
        assert!(fx.gh.pr_merge_calls().is_empty());
    }

    #[test]
    fn merge_strategy_stale_base_retry_merges_and_pushes_without_force() {
        let mut fx = up_to_date_fixture();
        fx.merge_cfg.strategy = MergeStrategy::Merge;
        fx.gh = fx
            .gh
            .with_pr_merge_sequence(vec![Err(stale_base_error()), Ok(())]);

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)), "{out}");
        assert_eq!(fx.git.merge_into_calls().len(), 1);
        assert!(fx.git.rebase_onto_calls().is_empty());
        assert_eq!(fx.git.push_calls().len(), 1);
        assert!(fx.git.push_force_with_lease_calls().is_empty());
    }

    #[test]
    fn merge_strategy_batch_session_uses_the_merge_prompt_and_polls_merges() {
        let mut fx = batch_fixture(&[
            BatchTicket::Conflict { resolves: true },
            BatchTicket::Conflict { resolves: true },
        ]);
        fx.merge_cfg.strategy = MergeStrategy::Merge;
        fx.git = fx
            .git
            .with_merge_into_result(Ok(RebaseOutcome::Conflicted))
            // Both tickets in progress on the first tick, done on the next.
            .with_merge_in_progress_sequence(vec![Ok(true), Ok(true), Ok(false)]);

        let (result, out) = run_batch(&fx.deps(), &keys(2));

        let report = result.expect("batch should succeed");
        assert_eq!(report.merged, keys(2), "{out}");
        let prompt = batch_prompt(&fx);
        assert!(prompt.contains("git commit --no-edit"), "{prompt}");
        assert!(
            prompt.contains("- PROJ-1: origin/main into branch proj-1-fix in "),
            "{prompt}"
        );
        assert_eq!(fx.git.push_calls().len(), 2);
        assert!(fx.git.push_force_with_lease_calls().is_empty());
        assert!(fx.git.rebase_onto_calls().is_empty());
        assert!(
            out.contains("PROJ-1: merge stopped on conflicts; set aside"),
            "{out}"
        );
        assert!(!out.contains("rebase stopped"), "{out}");
    }

    #[test]
    fn merge_strategy_without_a_checkout_says_it_skips_the_merge() {
        let mut fx = Fixture::new();
        fx.merge_cfg.strategy = MergeStrategy::Merge;
        fx.gh = fx
            .gh
            .with_pr_list(Ok(vec![pr_info(7, "proj-1-fix", "main", "PROJ-1")]));
        fx.git = fx.git.with_branch_exists_local(Ok(false));

        let (_result, out) = run(&fx.deps(), "PROJ-1");

        assert!(out.contains("skipping auto-merge"), "{out}");
    }

    // --- checks gate and verify_command (GitHub issue #241) ---

    fn checks_on(sha: &str, checks: Vec<CheckResult>) -> Result<PrChecks, GhError> {
        Ok(PrChecks {
            lifecycle: crate::github::gh_cli::PrLifecycle::Open,
            head_sha: sha.to_string(),
            checks,
        })
    }

    /// A ticket whose branch rebases cleanly onto `main`, then is pushed at
    /// head `aaa` (the fake's `rev_parse` never changes), with the checks
    /// gate on.
    fn gated_fixture() -> Fixture {
        let mut fx = Fixture::new();
        fx.register_checkout("proj-1-fix");
        fx.merge_cfg.wait_for_checks = true;
        fx.merge_cfg.checks_timeout_secs = 600;
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
        fx
    }

    #[test]
    fn failed_pre_push_checks_read_warns_and_still_waits() {
        let mut fx = gated_fixture();
        fx.gh = fx.gh.with_pr_checks_sequence(
            7,
            vec![
                Err(GhError::Command {
                    command: "gh pr view".to_string(),
                    exit_code: Some(1),
                    stderr: "connection reset".to_string(),
                }),
                checks_on("aaa", vec![]),
                checks_on("aaa", vec![check("test", ChecksState::Passing)]),
            ],
        );

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)), "{out}");
        assert!(out.contains("warning: could not read checks"), "{out}");
        assert_eq!(fx.git.push_force_with_lease_calls().len(), 1);
        // The empty list was read as "not queued yet", not "no CI".
        assert_eq!(fx.gh.pr_checks_calls().len(), 3);
    }

    #[test]
    fn pushed_head_with_passing_checks_merges() {
        let mut fx = gated_fixture();
        fx.gh = fx.gh.with_pr_checks_sequence(
            7,
            vec![
                checks_on("old", vec![check("test", ChecksState::Passing)]),
                checks_on("aaa", vec![check("test", ChecksState::Passing)]),
            ],
        );

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)), "{out}");
        assert_eq!(fx.git.push_force_with_lease_calls().len(), 1);
        assert_eq!(fx.gh.pr_merge_calls().len(), 1);
        assert!(out.contains("checks passed"), "{out}");
    }

    #[test]
    fn pending_checks_on_the_pushed_head_are_polled_until_they_pass() {
        let mut fx = gated_fixture();
        fx.gh = fx.gh.with_pr_checks_sequence(
            7,
            vec![
                checks_on("old", vec![check("test", ChecksState::Passing)]),
                checks_on("aaa", vec![check("test", ChecksState::Pending)]),
                checks_on("aaa", vec![check("test", ChecksState::Pending)]),
                checks_on("aaa", vec![check("test", ChecksState::Passing)]),
            ],
        );

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)), "{out}");
        assert_eq!(fx.sleeper.calls(), vec![CHECKS_POLL_SECS; 2]);
        assert_eq!(fx.gh.pr_merge_calls().len(), 1);
        assert!(out.contains("waiting for checks"), "{out}");
    }

    #[test]
    fn green_checks_on_the_old_head_do_not_count_for_the_pushed_one() {
        // GitHub can report the pre-push head (and its green checks) for a
        // moment after the push lands.
        let mut fx = gated_fixture();
        fx.gh = fx.gh.with_pr_checks_sequence(
            7,
            vec![
                checks_on("old", vec![check("test", ChecksState::Passing)]),
                checks_on("old", vec![check("test", ChecksState::Passing)]),
                checks_on("aaa", vec![check("test", ChecksState::Passing)]),
            ],
        );

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)), "{out}");
        assert_eq!(fx.sleeper.calls().len(), 1, "one wait for the new head");
    }

    #[test]
    fn checks_that_ran_before_the_push_are_awaited_on_the_pushed_head() {
        // The old head had checks, so an empty list on the new head means
        // they haven't been queued yet, not that the repo has no CI.
        let mut fx = gated_fixture();
        fx.gh = fx.gh.with_pr_checks_sequence(
            7,
            vec![
                checks_on("old", vec![check("test", ChecksState::Passing)]),
                checks_on("aaa", Vec::new()),
                checks_on("aaa", vec![check("test", ChecksState::Passing)]),
            ],
        );

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)), "{out}");
        assert_eq!(fx.sleeper.calls().len(), 1);
    }

    #[test]
    fn a_pr_with_no_checks_merges_without_waiting() {
        let mut fx = gated_fixture();
        fx.gh = fx.gh.with_pr_checks_sequence(
            7,
            vec![checks_on("old", Vec::new()), checks_on("aaa", Vec::new())],
        );

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)), "{out}");
        assert!(fx.sleeper.calls().is_empty());
        assert_eq!(fx.gh.pr_merge_calls().len(), 1);
    }

    #[test]
    fn failing_checks_on_the_pushed_head_stop_the_merge_and_name_them() {
        let mut fx = gated_fixture();
        fx.gh = fx.gh.with_pr_checks_sequence(
            7,
            vec![
                checks_on("old", vec![check("test", ChecksState::Passing)]),
                checks_on(
                    "aaa",
                    vec![
                        check("lint", ChecksState::Passing),
                        check("test", ChecksState::Failing),
                        check("build", ChecksState::Pending),
                    ],
                ),
            ],
        );

        let (result, out) = run(&fx.deps(), "PROJ-1");

        match result {
            Err(MergeError::ChecksFailed { number, checks }) => {
                assert_eq!(number, 7);
                assert_eq!(checks, vec!["test".to_string()]);
            }
            other => panic!("expected ChecksFailed, got {other:?}: {out}"),
        }
        assert_eq!(
            fx.git.push_force_with_lease_calls().len(),
            1,
            "the branch stays pushed"
        );
        assert!(fx.gh.pr_merge_calls().is_empty(), "nothing merged");
        assert!(fx.sleeper.calls().is_empty(), "a failure stops at once");
    }

    #[test]
    fn checks_still_pending_at_the_timeout_hand_the_ticket_back_unmerged() {
        let mut fx = gated_fixture();
        fx.clock = FakeClock::advancing(1_000, 200);
        fx.gh = fx.gh.with_pr_checks_sequence(
            7,
            vec![
                checks_on("old", vec![check("test", ChecksState::Passing)]),
                checks_on("aaa", vec![check("test", ChecksState::Pending)]),
            ],
        );

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(
            matches!(result, Ok(MergeFlowOutcome::ChecksPending)),
            "{result:?}: {out}"
        );
        assert!(fx.gh.pr_merge_calls().is_empty(), "nothing merged");
        assert_eq!(fx.git.push_force_with_lease_calls().len(), 1);
        assert!(out.contains("timed out"), "{out}");
    }

    #[test]
    fn failing_checks_stop_a_merge_that_pushed_nothing() {
        // An up-to-date branch is never pushed, but a rerun after a
        // checks timeout lands here: its head still has to be green.
        let mut fx = up_to_date_fixture();
        fx.merge_cfg.wait_for_checks = true;
        fx.gh = fx.gh.with_pr_checks(
            7,
            checks_on("aaa", vec![check("test", ChecksState::Failing)]),
        );

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(
            matches!(result, Err(MergeError::ChecksFailed { .. })),
            "{result:?}: {out}"
        );
        assert!(fx.git.push_force_with_lease_calls().is_empty());
        assert!(fx.gh.pr_merge_calls().is_empty());
    }

    #[test]
    fn checks_gate_off_never_reads_checks() {
        let mut fx = gated_fixture();
        fx.merge_cfg.wait_for_checks = false;

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)), "{out}");
        assert!(fx.gh.pr_checks_calls().is_empty());
    }

    #[test]
    fn stale_base_retry_waits_for_checks_on_the_repushed_head() {
        let mut fx = up_to_date_fixture();
        fx.merge_cfg.wait_for_checks = true;
        fx.gh = fx
            .gh
            .with_pr_merge_sequence(vec![Err(stale_base_error()), Ok(())])
            .with_pr_checks_sequence(
                7,
                vec![
                    // The up-to-date head before the first merge attempt.
                    checks_on("aaa", vec![check("test", ChecksState::Passing)]),
                    // Before the retry push, then pending on the new head.
                    checks_on("aaa", vec![check("test", ChecksState::Passing)]),
                    checks_on("aaa", vec![check("test", ChecksState::Pending)]),
                    checks_on("aaa", vec![check("test", ChecksState::Passing)]),
                ],
            );

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)), "{out}");
        assert_eq!(fx.gh.pr_checks_calls().len(), 4);
        assert_eq!(fx.sleeper.calls(), vec![CHECKS_POLL_SECS]);
        assert_eq!(fx.gh.pr_merge_calls().len(), 2);
    }

    #[test]
    fn verify_command_runs_in_the_checkout_before_the_push() {
        let mut fx = gated_fixture();
        fx.merge_cfg.wait_for_checks = false;
        fx.merge_cfg.verify_command = Some("cargo test".to_string());
        let dir = fx._worktree_dirs[0].path().to_path_buf();

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)), "{out}");
        assert_eq!(
            fx.verify_calls.borrow().clone(),
            vec![(dir, "cargo test".to_string())]
        );
        assert_eq!(fx.git.push_force_with_lease_calls().len(), 1);
    }

    #[test]
    fn failing_verify_command_stops_the_merge_with_nothing_pushed() {
        let mut fx = gated_fixture();
        fx.merge_cfg.verify_command = Some("cargo test".to_string());
        fx.verify_passes.set(false);

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(
            matches!(result, Err(MergeError::VerifyFailed { .. })),
            "{result:?}: {out}"
        );
        assert!(fx.git.push_force_with_lease_calls().is_empty());
        assert!(fx.gh.pr_merge_calls().is_empty());
    }

    #[test]
    fn failing_verify_command_names_its_log_under_state_dir() {
        let mut fx = gated_fixture();
        fx.merge_cfg.verify_command = Some("cargo test".to_string());
        fx.verify_passes.set(false);

        let (result, out) = run(&fx.deps(), "PROJ-1");

        match result {
            Err(MergeError::VerifyFailed { log, .. }) => {
                assert_eq!(log, fx.state_dir.join("merge-PROJ-1-verify.log"));
            }
            other => panic!("{other:?}: {out}"),
        }
    }

    #[test]
    fn run_verify_command_writes_large_output_to_the_log_not_tm_streams() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("verify.log");
        // ~200 KB across stdout and stderr: well past a 64 KB pipe buffer.
        let command = "i=0; while [ $i -lt 2000 ]; do \
            printf '%0100d\\n' $i; printf '%0100d\\n' $i >&2; i=$((i+1)); done; \
            read -r line || exit 0";

        let passed = run_verify_command(dir.path(), command, &log).unwrap();

        assert!(passed, "stdin should be closed, so `read` hits EOF");
        let len = std::fs::metadata(&log).unwrap().len();
        assert!(len > 64 * 1024, "log holds {len} bytes");
    }

    #[test]
    fn verify_command_is_skipped_when_nothing_is_pushed() {
        let mut fx = up_to_date_fixture();
        fx.merge_cfg.verify_command = Some("cargo test".to_string());

        let (result, out) = run(&fx.deps(), "PROJ-1");

        assert!(matches!(result, Ok(MergeFlowOutcome::Merged)), "{out}");
        assert!(fx.verify_calls.borrow().is_empty());
    }

    #[test]
    fn batch_applies_the_checks_gate_to_each_ticket() {
        let mut fx = batch_fixture(&[BatchTicket::Clean, BatchTicket::Clean, BatchTicket::Clean]);
        fx.merge_cfg.wait_for_checks = true;
        fx.merge_cfg.checks_timeout_secs = 600;
        fx.clock = FakeClock::advancing(1_000, 200);
        fx.gh = fx
            .gh
            .with_pr_checks_sequence(
                11,
                vec![
                    checks_on("old-1", Vec::new()),
                    checks_on("old-1", vec![check("test", ChecksState::Failing)]),
                ],
            )
            .with_pr_checks_sequence(
                12,
                vec![
                    checks_on("x", Vec::new()),
                    checks_on("old-2", vec![check("test", ChecksState::Pending)]),
                ],
            )
            .with_pr_checks_sequence(
                13,
                vec![
                    checks_on("x", Vec::new()),
                    checks_on("old-3", vec![check("test", ChecksState::Passing)]),
                ],
            );

        let (result, out) = run_batch(&fx.deps(), &keys(3));

        let report = result.expect("batch should succeed");
        assert_eq!(report.merged, vec!["PROJ-3"], "{out}");
        assert_eq!(report.checks_pending, vec!["PROJ-2"], "{out}");
        assert_eq!(report.failed.len(), 1, "{out}");
        assert_eq!(report.failed[0].0, "PROJ-1");
        assert_eq!(report.exit_code(), 1);
        assert_eq!(fx.gh.pr_merge_calls(), vec![(PathBuf::from("/repo"), 13)]);
        assert!(out.contains("checks pending: PROJ-2"), "{out}");
    }

    #[test]
    fn batch_with_only_pending_checks_exits_two() {
        let report = BatchMergeReport {
            checks_pending: vec!["PROJ-1".to_string()],
            ..Default::default()
        };
        assert_eq!(report.exit_code(), 2);
    }
}
