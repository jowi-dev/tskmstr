//! `gh` CLI wrapper for looking up, creating, and editing GitHub pull
//! requests.
//!
//! [`GhCli`] is the trait callers depend on; [`ShellGhCli`] is the
//! `gh`/`git`-shelling-out implementation used in production. [`FakeGhCli`]
//! is a test double for use by tests that don't want to shell out.
//!
//! `pr_create` and `pr_edit` shell out and then re-interpret exit
//! code/stderr through small pure helpers, the same pattern used by
//! `pr_view` and `current_branch`, so the interesting logic is unit
//! testable without a real `gh` binary. There is no automated end-to-end
//! test of [`ShellGhCli`] itself: `gh` requires a real GitHub repository and
//! an open pull request to exercise meaningfully, so that coverage is
//! deliberately left to manual/E2E verification rather than an `#[ignore]`d
//! test here.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;

use super::bot_findings::{FindingDetail, PrReview, ReviewThread};
use super::pr::PrInfo;

/// Errors that can occur while shelling out to `gh` or `git`.
#[derive(Debug, Clone, Error)]
pub enum GhError {
    /// The `gh` or `git` binary could not be spawned.
    #[error("failed to run `{command}`: {message}")]
    Spawn {
        /// The command that could not be spawned, e.g. `gh pr view`.
        command: String,
        /// The underlying spawn error message.
        message: String,
    },

    /// The command ran but exited with a failure not otherwise categorized.
    #[error("`{command}` failed (exit {exit_code:?}): {stderr}")]
    Command {
        /// The command that failed, e.g. `gh pr view`.
        command: String,
        /// The process exit code, if the process was not terminated by a signal.
        exit_code: Option<i32>,
        /// Captured stderr.
        stderr: String,
    },

    /// The command succeeded but its output could not be parsed.
    #[error("failed to parse `{command}` output: {message}")]
    Parse {
        /// The command whose output failed to parse.
        command: String,
        /// The underlying parse error message.
        message: String,
    },

    /// The command did not finish within its bounded wait and was killed.
    ///
    /// Only ever produced by [`GhCli::pr_list_bounded`] (see its doc comment
    /// for why that's the one method on this trait with a bound at all) --
    /// every other method here shells out with an ordinary unbounded
    /// `.output()` call, matching `tm`'s existing "a CLI command taking
    /// longer than usual is annoying but not board-freezing" tolerance.
    #[error("`{command}` did not finish within {seconds}s and was killed")]
    Timeout {
        /// The command that timed out, e.g. `gh pr list`.
        command: String,
        /// The bound that was exceeded, in seconds.
        seconds: u64,
    },
}

impl GhError {
    /// Whether this is a **permanent** failure — a bug in how *tm* itself
    /// calls `gh` (an invalid `--json` field, an unknown flag, an unknown
    /// subcommand) — as opposed to a **transient** environmental condition
    /// (network error, rate limit, a `5xx`, expired auth, `gh` missing, a
    /// timeout).
    ///
    /// This distinction exists because of a real incident: an earlier
    /// version of this code requested an invalid `--json` field
    /// (`"merged"`, see [`PrLifecycle`]'s doc comment) and every call failed
    /// identically, forever, with `Unknown JSON field: "merged"` on stderr —
    /// a permanent, code-level defect. But callers that catch a `gh` error
    /// and fall back rather than fail (e.g.
    /// [`crate::work::run::resolve_blocker_stacking`], designed to tolerate
    /// a *transient* network hiccup) had no way to tell that failure apart
    /// from an ordinary blip, so the permanent bug was silently swallowed as
    /// a "warning" that went nowhere visible — six autonomous lane runs were
    /// dispatched against the wrong (unstacked) base as a result, each
    /// burning real cost. `is_permanent` is what lets those call sites
    /// refuse to pretend a permanent failure is a transient one.
    ///
    /// Detection is narrow and stderr-driven, matching only wording `gh`
    /// itself uses to say "you asked me something nonsensical" — see
    /// [`is_permanent_stderr`]. It only ever inspects [`GhError::Command`];
    /// [`GhError::Spawn`] (couldn't even launch `gh` — e.g. not installed,
    /// which is an environment problem) and [`GhError::Parse`] (`gh`
    /// succeeded but its output didn't parse — ambiguous, could be either)
    /// always classify as transient.
    ///
    /// Deliberately biased toward **transient by default**: every case not
    /// explicitly matched — including every `Command` error whose stderr
    /// doesn't hit one of the known markers — is transient. A misclassified
    /// transient error only costs an unnecessary warn-and-fallback; a
    /// misclassified permanent error would wrongly hard-fail a run over an
    /// ordinary network blip, which is the failure mode this default avoids.
    pub fn is_permanent(&self) -> bool {
        match self {
            GhError::Command { stderr, .. } => is_permanent_stderr(stderr),
            GhError::Spawn { .. } | GhError::Parse { .. } | GhError::Timeout { .. } => false,
        }
    }
}

/// Narrow, case-insensitive check for the specific wording `gh` uses when
/// *tm* itself called it wrong, rather than when the environment
/// misbehaved:
///
/// - `Unknown JSON field: "..."` — an invalid `--json` field name (the
///   incident this whole distinction exists for).
/// - `unknown flag: ...` / `unknown shorthand flag: ...` — a flag `gh`'s
///   version doesn't recognize.
/// - `unknown command "..." for "..."` — a subcommand `gh` doesn't
///   recognize.
///
/// Kept intentionally narrow (see [`GhError::is_permanent`]'s doc comment
/// for why): anything not matching one of these falls through to transient,
/// which is the safe default.
fn is_permanent_stderr(stderr: &str) -> bool {
    let lower = stderr.to_lowercase();
    lower.contains("unknown json field")
        || lower.contains("unknown flag")
        || lower.contains("unknown shorthand flag")
        || lower.contains("unknown command")
}

/// A short, appendable clause flagging a permanent [`GhError`] loudly in a
/// warning line, empty for a transient one.
///
/// For every best-effort "catch a `gh` error and degrade to a fallback"
/// call site that isn't important enough to hard-fail on (unlike
/// [`crate::work::run::resolve_blocker_stacking`] or
/// [`crate::work::review_watch::run_poll_loop`], which do), this is the
/// minimum fix for the other half of the incident this whole
/// permanent/transient distinction exists for: a warning that reads exactly
/// the same for "the network hiccuped" and "tm has had a code-level bug for
/// months" is not "surfaced prominently" — see
/// [`GhError::is_permanent`]'s doc comment. Appending this clause is enough
/// to tell a reader "this isn't going to fix itself; someone should file an
/// issue" without changing what the call site does (still prints a warning
/// and continues on the happy path — these are advisory annotations on an
/// otherwise-successful command, not an autonomous run's only chance to
/// notice the failure, so a hard fail here would be disproportionate).
pub fn permanence_note(err: &GhError) -> &'static str {
    if err.is_permanent() {
        " (this looks like a bug in tm itself, not a network/gh issue — it will not resolve on retry)"
    } else {
        ""
    }
}

/// How often [`spawn_with_timeout`]'s poll loop checks whether the child has
/// exited. Small enough that the loop's own overhead never meaningfully
/// stretches the timeout bound it's enforcing, large enough not to spin the
/// CPU while waiting on a slow `gh` call.
const POLL_STEP: Duration = Duration::from_millis(25);

/// Run `command` to completion, but kill it and return
/// `Err(GhError::Timeout)` if it hasn't exited within `timeout`.
///
/// `label` is a human-readable name for the command (e.g. `"gh pr list"`),
/// used only in the error variants' messages -- `command` itself isn't
/// `Debug`, so it can't be interpolated directly.
///
/// stdout/stderr are drained on background threads *while* the poll loop
/// runs, not read afterward: `gh pr list` on a repository with many open PRs
/// can produce enough JSON to fill a pipe's OS buffer, and reading it only
/// after the child exits (the way [`std::process::Command::output`] would
/// look if reimplemented naively here) would deadlock a child that's blocked
/// writing to a full pipe while this function is blocked waiting for it to
/// exit. This mirrors what `Command::output`/`wait_with_output` already do
/// internally; the only reason this function exists instead of calling
/// `wait_with_output` on a spawned child is that `wait_with_output` has no
/// bound and can't be interrupted once called.
///
/// Used only by [`GhCli::pr_list_bounded`], but kept generic over `Command`
/// (rather than hardcoded to build the `gh pr list` invocation itself) so it
/// can be unit-tested directly against ordinary subprocesses (`sleep`,
/// `echo`) without needing a real `gh` binary or network -- see this
/// module's tests.
fn spawn_with_timeout(
    mut command: Command,
    label: &str,
    timeout: Duration,
) -> Result<std::process::Output, GhError> {
    use std::io::Read;
    use std::process::Stdio;

    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|err| GhError::Spawn {
        command: label.to_string(),
        message: err.to_string(),
    })?;

    let mut stdout_pipe = child.stdout.take().expect("stdout was piped above");
    let mut stderr_pipe = child.stderr.take().expect("stderr was piped above");
    let stdout_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
    });

    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|err| GhError::Spawn {
            command: label.to_string(),
            message: err.to_string(),
        })? {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(GhError::Timeout {
                command: label.to_string(),
                seconds: timeout.as_secs(),
            });
        }
        std::thread::sleep(POLL_STEP);
    };

    let stdout = stdout_thread.join().unwrap_or_default();
    let stderr = stderr_thread.join().unwrap_or_default();
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

/// The lifecycle state of a pull request, as reported by `gh pr view --json
/// state`.
///
/// `gh`'s `state` field reports `OPEN`, `CLOSED`, or `MERGED` directly (there
/// is no separate `merged` boolean field on `gh pr view`/`gh pr list` — an
/// earlier version of this code requested one and every call failed with
/// `Unknown JSON field: "merged"`), so `state` alone is sufficient to tell
/// "closed because merged" apart from "closed unmerged".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrLifecycle {
    /// The pull request is open.
    Open,
    /// The pull request was merged.
    Merged,
    /// The pull request was closed without merging.
    Closed,
}

/// Request body for creating a pull request via `gh pr create`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrCreateRequest {
    /// Pull request title.
    pub title: String,
    /// Pull request body (description).
    pub body: String,
    /// Base branch to open the PR against, if not the repository default.
    pub base: Option<String>,
}

/// Request body for editing a pull request via `gh pr edit`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PrEditRequest {
    /// New title, if it should change.
    pub title: Option<String>,
    /// New body, if it should change.
    pub body: Option<String>,
}

/// Behavior tskmstr needs from the `gh` CLI (and `git`, for branch lookup).
pub trait GhCli {
    /// Look up the pull request open for the current branch
    /// (`gh pr view --json ...`).
    ///
    /// Returns `Ok(None)` when there is no pull request for the current
    /// branch, rather than treating that as an error.
    fn pr_view(&self) -> Result<Option<PrInfo>, GhError>;

    /// Create a pull request (`gh pr create`).
    fn pr_create(&self, req: &PrCreateRequest) -> Result<PrInfo, GhError>;

    /// Edit an existing pull request (`gh pr edit`).
    fn pr_edit(&self, number: u64, req: &PrEditRequest) -> Result<(), GhError>;

    /// Post a comment to a pull request (`gh pr comment <number> --body
    /// <text>`).
    ///
    /// `body` is plain (GitHub-flavored) Markdown, unlike
    /// [`crate::jira::client::JiraClient::add_comment`]'s ADF body — GitHub
    /// comments are Markdown natively, so no conversion happens on this path.
    fn pr_comment(&self, number: u64, body: &str) -> Result<(), GhError>;

    /// The name of the currently checked out branch (`git branch --show-current`).
    fn current_branch(&self) -> Result<String, GhError>;

    /// List the review threads on pull request `number`, including each
    /// thread's resolution state and first-comment author.
    ///
    /// Thread resolution state is only exposed via GitHub's GraphQL API, not
    /// the REST endpoints `gh` otherwise wraps, so this shells out to
    /// `gh api graphql`. Only the first 100 review threads are fetched;
    /// pagination is not followed. This mirrors the single-page limitation
    /// documented on [`crate::jira::client::JiraClient::search`] and is
    /// sufficient for tskmstr's current use (bot review findings rarely
    /// exceed one page of threads).
    ///
    /// `dir` is an explicit repo root, shelled out against via
    /// `.current_dir(dir)`, for the same reason documented on
    /// [`GhCli::pr_list_all`]: callers (`tm pr status`, `tm ready`, and `tm pr
    /// watch`'s poll loop) don't all run with the target repo as the ambient
    /// cwd, and this method's own `gh repo view` owner/repo resolution would
    /// silently resolve the wrong repository (or fail outright) otherwise.
    fn pr_review_threads(&self, dir: &Path, number: u64) -> Result<Vec<ReviewThread>, GhError>;

    /// List the review submissions on pull request `number`, one entry per
    /// review regardless of how many comments (if any) it left.
    ///
    /// Unlike [`GhCli::pr_review_threads`] (GraphQL-only, unsuffixed bot
    /// logins), this shells out to `gh api
    /// repos/{owner}/{repo}/pulls/{number}/reviews` (REST), which reports bot
    /// logins *with* the `[bot]` suffix. This is the only way to see that a
    /// bot has run at all: a bot that finds nothing posts a review with zero
    /// comments, leaving no trace in review-thread data.
    ///
    /// `dir` is an explicit repo root, same rationale as
    /// [`GhCli::pr_review_threads`]'s doc comment — this method's `tm pr
    /// watch` poll-loop caller runs detached, not necessarily inside the
    /// target repo.
    fn pr_reviews(&self, dir: &Path, number: u64) -> Result<Vec<PrReview>, GhError>;

    /// The lifecycle state of pull request `number` (`gh pr view <number>
    /// --json state`).
    ///
    /// `dir` is an explicit repo root, same rationale as
    /// [`GhCli::pr_review_threads`]'s doc comment.
    fn pr_state(&self, dir: &Path, number: u64) -> Result<PrLifecycle, GhError>;

    /// List the review threads on pull request `number`, same as
    /// [`GhCli::pr_review_threads`] but with each thread's first comment's
    /// full body/path/line/URL, for handing bot findings to a cleanup
    /// session prompt. A separate GraphQL query from
    /// [`GhCli::pr_review_threads`]'s: that counting path never needed
    /// comment bodies/locations, so this doesn't touch it.
    ///
    /// `dir` is an explicit repo root, same rationale as
    /// [`GhCli::pr_review_threads`]'s doc comment.
    fn pr_bot_finding_details(
        &self,
        dir: &Path,
        number: u64,
    ) -> Result<Vec<FindingDetail>, GhError>;

    /// List open pull requests in the repository rooted at `dir`, fetching
    /// the same fields as [`GhCli::pr_view`] (`gh pr list --state open
    /// --limit 200 --json number,url,title,body,headRefName`) so one
    /// [`PrInfo`] parser serves both and callers like
    /// [`super::pr::find_pr_for_ticket`] have the title/body/branch data they
    /// need to resolve a ticket key without a second lookup.
    ///
    /// `dir` is an explicit repo root, shelled out against via
    /// `.current_dir(dir)`, same rationale as [`GhCli::pr_list_all`]'s doc
    /// comment: `tm pr watch`'s ticket-to-PR resolution is the original
    /// motivating case — the board and the detached `--foreground` child both
    /// run with an ambient cwd that is not reliably the ticket's repo. `tm
    /// ready` and `tm pr status` pass their own process cwd, unaffected in
    /// the normal case of running `tm` from inside the repo. `--limit 200`
    /// (rather than `gh`'s default of 30) so a repo with more than 30 open
    /// PRs doesn't silently hide the one being searched for.
    fn pr_list(&self, dir: &Path) -> Result<Vec<PrInfo>, GhError>;

    /// Same as [`GhCli::pr_list`], but bounded by `timeout`: if the `gh`
    /// child process hasn't finished within it, it is killed and
    /// `Err(GhError::Timeout)` is returned instead of blocking forever.
    ///
    /// Exists for exactly one caller: `crate::tui::event::resolve_pr_for_ticket`,
    /// the board's `o`-key PR lookup. That call runs synchronously inside the
    /// terminal event loop with no way to cancel a hung child -- a dead
    /// network or expired `gh` auth would otherwise freeze the whole board
    /// indefinitely (no redraw, no key input, not even quit). Every other
    /// caller of PR data (`tm pr watch`, `tm pr status`, `tm ready`) keeps
    /// calling the unbounded [`GhCli::pr_list`] unchanged: those are batch CLI
    /// commands where a slow `gh` call is merely annoying, not a frozen UI, so
    /// there is no reason to newly bound them.
    fn pr_list_bounded(&self, dir: &Path, timeout: Duration) -> Result<Vec<PrInfo>, GhError>;

    /// The login of the currently authenticated `gh` user
    /// (`gh api user -q .login`), used by `tm work run`'s branch-owner
    /// resolution (see `crate::work::run`).
    ///
    /// Returns `Ok(None)` when `gh` isn't authenticated, isn't installed, or
    /// otherwise fails to resolve a login — this is a "best effort" lookup
    /// with fallbacks above it in the branch-owner chain, not a hard
    /// dependency, so a failure here is not an error condition.
    fn current_user_login(&self) -> Result<Option<String>, GhError>;

    /// The URL of the open pull request for `branch`, if any (`gh pr list
    /// --head <branch> --json url -q '.[0].url'`), used by `tm work run`'s
    /// post-run PR-URL resolution (see `crate::work::run::run_claude_and_finish`).
    ///
    /// Returns `Ok(None)` when there is no open pull request for `branch`,
    /// `gh` isn't installed, or the lookup otherwise fails — a missing PR is
    /// normal (most runs don't open one), not an error condition, mirroring
    /// `work.ml`'s tolerant `gh pr list ... 2>>'log'` call.
    fn pr_url_for_branch(&self, branch: &str) -> Result<Option<String>, GhError>;

    /// List every pull request in the current repository, open *or* merged
    /// (`gh pr list --state all --json number,headRefName,state,updatedAt`).
    ///
    /// Used by `tm work run`'s blocked-ticket branch-off logic (see
    /// [`crate::work::run::resolve_blocker_stacking`]) to find and classify
    /// the PR (if any) for a blocking ticket's lane branch. Unlike
    /// [`GhCli::pr_list`] (open-only, used elsewhere for ticket-branch
    /// discovery), this must include merged PRs too — a blocker's PR being
    /// merged is exactly the "already in staging, nothing to stack on"
    /// outcome that logic needs to distinguish from "still open, unmerged".
    /// No head-branch filter is applied server-side (`gh pr list` has none
    /// for "starts with"); callers filter the returned list themselves.
    ///
    /// Unlike this trait's other methods (all of which shell out with the
    /// tm process's ambient cwd, since they're only ever called from `tm`
    /// subcommands already running inside the target repo), `dir` is an
    /// explicit repo root: `tm work run` resolves blockers for
    /// `lane_config.repo`, which is generally *not* the invoking process's
    /// cwd (e.g. running a lane from another directory, or from the board).
    /// Shelling out with the ambient cwd would silently list PRs for the
    /// wrong repository — or fail outright with "not a git repository" —
    /// corrupting blocker resolution without any obviously-related error.
    fn pr_list_all(&self, dir: &Path) -> Result<Vec<PrSummary>, GhError>;

    // --- Issue operations (phase 4 of the GitHub-Issues-as-a-backend work,
    // docs/plans/github-issues-backend.md) ---
    //
    // Every method below takes `repo: &str`, an `"owner/name"` slug, rather
    // than a `dir: &Path` the way the `pr_*` methods above do. That's a
    // deliberate departure: the `pr_*` methods resolve their target
    // repository from a git checkout (`dir`'s remote, via `gh repo view`),
    // because a PR only exists relative to a branch that's actually checked
    // out somewhere. `GithubProvider` (phase 5/6) has no such checkout to
    // anchor on — it's driven entirely by `[backend].repo` from config
    // (`docs/plans/github-issues-backend.md`'s "Carry-forward decisions"),
    // and the board or a lane run may target that repo without it being the
    // invoking process's cwd, or checked out locally at all. Passing the
    // slug straight through as `-R owner/name` sidesteps needing a checkout
    // for every issue read/write, which `dir`-based `gh repo view`
    // resolution would otherwise require.

    /// Look up issue `number` in `repo` (`gh issue view <number> -R <repo>
    /// --json ...`).
    fn issue_view(&self, repo: &str, number: u64) -> Result<IssueInfo, GhError>;

    /// List issues in `repo` matching `filter` (`gh issue list -R <repo>
    /// --state ... --json ...`).
    ///
    /// `filter.limit` is always passed explicitly as `--limit`: `gh issue
    /// list`, like `gh pr list`, defaults to 30, which would silently hide
    /// issues in a busy repo.
    fn issue_list(&self, repo: &str, filter: &IssueListFilter) -> Result<Vec<IssueInfo>, GhError>;

    /// Create an issue in `repo` (`gh issue create -R <repo> --title ...
    /// --body ...`).
    ///
    /// `gh issue create` prints only the created issue's URL on success (no
    /// `--json` support, same as `gh pr create`); the returned [`IssueInfo`]
    /// is fetched with a follow-up [`GhCli::issue_view`] call, the same
    /// two-step pattern [`GhCli::pr_create`] uses via [`GhCli::pr_view`].
    fn issue_create(&self, repo: &str, req: &IssueCreateRequest) -> Result<IssueInfo, GhError>;

    /// Edit issue `number` in `repo`: label/assignee changes via `gh issue
    /// edit -R <repo> --add-label/--remove-label/--add-assignee/
    /// --remove-assignee`, and, if `req.state` is set, a follow-up `gh issue
    /// close`/`gh issue reopen -R <repo>`.
    ///
    /// These are two separate `gh` subcommands under the hood (`gh issue
    /// edit` has no close/reopen flag), issued in that order when both a
    /// label/assignee change and a state change are requested. This mirrors
    /// the design doc's transition model: "a label swap ... plus, for
    /// Done/Reopen, a close/reopen" is exactly two `gh` calls, not one.
    fn issue_edit(&self, repo: &str, number: u64, req: &IssueEditRequest) -> Result<(), GhError>;

    /// Post a comment to issue `number` in `repo` (`gh issue comment
    /// <number> -R <repo> --body <text>`).
    ///
    /// `body` is plain (GitHub-flavored) Markdown, same as
    /// [`GhCli::pr_comment`] — no ADF conversion happens on this path.
    fn issue_comment(&self, repo: &str, number: u64, body: &str) -> Result<(), GhError>;

    /// Fetch issue `number`'s native GitHub issue dependencies in `repo`:
    /// the issues it's blocked by, and the issues it blocks.
    ///
    /// Like [`GhCli::pr_review_threads`], this data is only exposed via
    /// GraphQL, not the REST endpoints `gh` otherwise wraps, so this shells
    /// out to `gh api graphql`. Only the first 100 issues on each side are
    /// fetched, the same single-page limitation as
    /// [`GhCli::pr_review_threads`].
    fn issue_dependencies(&self, repo: &str, number: u64) -> Result<IssueDependencies, GhError>;
}

/// A summary of one pull request — its number, head branch, lifecycle state,
/// and last-updated timestamp — as returned by [`GhCli::pr_list_all`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrSummary {
    /// The pull request's number.
    pub number: u64,
    /// The pull request's head branch name.
    pub head_ref_name: String,
    /// The pull request's lifecycle state.
    pub lifecycle: PrLifecycle,
    /// `gh`'s reported `updatedAt`, an ISO-8601 timestamp. Only used to break
    /// ties when more than one PR matches the same head-branch prefix (see
    /// [`crate::work::run::resolve_blocker_stacking`]'s "most recently
    /// updated" tiebreak) — lexical string comparison is sufficient since
    /// ISO-8601 timestamps sort chronologically as strings.
    pub updated_at: String,
}

/// The open/closed state of a GitHub issue, as reported by `state` in `gh
/// issue view`/`gh issue list --json` output (`"OPEN"` or `"CLOSED"`).
///
/// Unlike [`PrLifecycle`], there is no third `Merged` state — issues only
/// ever open or close.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueState {
    /// The issue is open.
    Open,
    /// The issue is closed.
    Closed,
}

/// A GitHub issue, as returned by [`GhCli::issue_view`] or [`GhCli::issue_list`].
///
/// `labels` and `assignees` are flattened to plain name/login strings from
/// `gh`'s richer `{name: ...}`/`{login: ...}` object shape — nothing in `tm`
/// needs a label's color or an assignee's display name, only the label
/// namespace strings (`tm:status/*`) and login used to join against
/// `runs.db`/branch naming.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueInfo {
    /// Issue number.
    pub number: u64,
    /// Web URL of the issue.
    pub url: String,
    /// Issue title.
    pub title: String,
    /// Issue body (description), plain Markdown.
    pub body: String,
    /// Open/closed state.
    pub state: IssueState,
    /// Label names attached to the issue.
    pub labels: Vec<String>,
    /// Logins of the issue's assignees.
    pub assignees: Vec<String>,
}

/// Which of an issue's open/closed state to request via `gh issue list
/// --state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueListState {
    /// Only open issues.
    Open,
    /// Only closed issues.
    Closed,
    /// Both open and closed issues.
    All,
}

impl IssueListState {
    /// The `--state` value `gh issue list` expects.
    fn as_gh_arg(self) -> &'static str {
        match self {
            IssueListState::Open => "open",
            IssueListState::Closed => "closed",
            IssueListState::All => "all",
        }
    }
}

/// Filter for [`GhCli::issue_list`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueListFilter {
    /// Which open/closed state to list.
    pub state: IssueListState,
    /// Restrict to issues carrying all of these labels. Empty means no
    /// label filter.
    pub labels: Vec<String>,
    /// Restrict to issues assigned to this login (`gh issue list
    /// --assignee`). `None` means no assignee filter.
    pub assignee: Option<String>,
    /// `--limit` passed to `gh issue list`. Always set explicitly (see
    /// [`GhCli::issue_list`]'s doc comment on why `gh`'s own default of 30
    /// isn't safe to rely on).
    pub limit: u32,
}

impl Default for IssueListFilter {
    /// Open issues, no label/assignee filter, `--limit 200` — the same
    /// bound [`GhCli::pr_list`] uses for the equivalent PR listing.
    fn default() -> Self {
        Self {
            state: IssueListState::Open,
            labels: Vec::new(),
            assignee: None,
            limit: 200,
        }
    }
}

/// Request body for creating an issue via [`GhCli::issue_create`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IssueCreateRequest {
    /// Issue title.
    pub title: String,
    /// Issue body (description), plain Markdown.
    pub body: String,
    /// Labels to attach at creation time.
    pub labels: Vec<String>,
    /// Logins to assign at creation time.
    pub assignees: Vec<String>,
}

/// A close/reopen state change requested as part of [`IssueEditRequest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueStateChange {
    /// Close the issue (`gh issue close`).
    Close,
    /// Reopen the issue (`gh issue reopen`).
    Reopen,
}

/// Request body for editing an issue via [`GhCli::issue_edit`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IssueEditRequest {
    /// Labels to add.
    pub add_labels: Vec<String>,
    /// Labels to remove.
    pub remove_labels: Vec<String>,
    /// Logins to add as assignees.
    pub add_assignees: Vec<String>,
    /// Logins to remove as assignees.
    pub remove_assignees: Vec<String>,
    /// Close or reopen the issue, if it should change.
    pub state: Option<IssueStateChange>,
}

/// A minimal reference to another issue, as returned inside
/// [`IssueDependencies`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueRef {
    /// The referenced issue's number.
    pub number: u64,
    /// The referenced issue's title.
    pub title: String,
    /// The referenced issue's open/closed state.
    pub state: IssueState,
    /// Web URL of the referenced issue.
    pub url: String,
}

/// The result of [`GhCli::issue_dependencies`]: the issues an issue is
/// blocked by, and the issues it blocks, via GitHub's native issue
/// dependencies feature.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IssueDependencies {
    /// Issues blocking this one.
    pub blocked_by: Vec<IssueRef>,
    /// Issues this one blocks.
    pub blocking: Vec<IssueRef>,
}

/// Fields requested from `gh pr view --json`; shared so the flag and the
/// [`PrInfo`] deserialization stay in lockstep.
const PR_VIEW_JSON_FIELDS: &str = "number,url,title,body,headRefName";

/// Fields requested from `gh pr view <number> --json` in [`GhCli::pr_state`];
/// shared so the flag and [`RawPrState`] deserialization stay in lockstep.
/// Deliberately just `state`, not `state,merged`: `gh pr view` has no
/// `merged` JSON field (see [`PrLifecycle`]'s doc comment), and requesting it
/// makes every call fail with `Unknown JSON field: "merged"`.
const PR_STATE_JSON_FIELDS: &str = "state";

/// Fields requested from `gh pr list --state all --json` in
/// [`GhCli::pr_list_all`]; shared so the flag and [`RawPrListAllEntry`]
/// deserialization stay in lockstep. Same `merged`-field pitfall as
/// [`PR_STATE_JSON_FIELDS`] applies here.
const PR_LIST_ALL_JSON_FIELDS: &str = "number,headRefName,state,updatedAt";

/// Fields requested from `gh issue view`/`gh issue list --json`; shared so
/// the flag and [`RawIssueView`] deserialization stay in lockstep.
const ISSUE_JSON_FIELDS: &str = "number,url,title,body,state,labels,assignees";

/// GraphQL query fetching an issue's native dependencies: the issues it's
/// blocked by, and the issues it blocks (see [`GhCli::issue_dependencies`]).
///
/// Fetches only the first 100 issues per side, same single-page limitation
/// as [`REVIEW_THREADS_QUERY`].
const ISSUE_DEPENDENCIES_QUERY: &str = "query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    issue(number: $number) {
      blockedBy(first: 100) {
        nodes { number title state url }
      }
      blocking(first: 100) {
        nodes { number title state url }
      }
    }
  }
}";

/// GraphQL query fetching a pull request's review threads: resolution state
/// plus the author of each thread's first comment.
///
/// Fetches only the first 100 threads (see [`GhCli::pr_review_threads`]).
const REVIEW_THREADS_QUERY: &str = "query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviewThreads(first: 100) {
        nodes {
          isResolved
          comments(first: 1) { nodes { author { login } } }
        }
      }
    }
  }
}";

/// GraphQL query fetching a pull request's review threads' resolution state
/// plus each thread's first comment in full: author, body, path, line, and
/// URL. A separate query from [`REVIEW_THREADS_QUERY`], not an extension of
/// it (see [`GhCli::pr_bot_finding_details`]).
///
/// Fetches only the first 100 threads, same limitation as
/// [`REVIEW_THREADS_QUERY`] (see [`GhCli::pr_review_threads`]).
const FINDING_DETAILS_QUERY: &str = "query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviewThreads(first: 100) {
        nodes {
          isResolved
          comments(first: 1) { nodes { author { login } body path line url } }
        }
      }
    }
  }
}";

/// [`GhCli`] implementation that shells out to the real `gh` and `git`
/// binaries.
pub struct ShellGhCli;

impl ShellGhCli {
    /// Create a new shell-backed `gh` CLI wrapper.
    pub fn new() -> Self {
        Self
    }
}

impl Default for ShellGhCli {
    fn default() -> Self {
        Self::new()
    }
}

impl ShellGhCli {
    /// Resolve `dir`'s repository owner and name (`gh repo view --json
    /// owner,name`), shared by [`GhCli::pr_review_threads`],
    /// [`GhCli::pr_reviews`], and [`GhCli::pr_bot_finding_details`], all of
    /// which need it to build a REST/GraphQL path for the repository. `dir`
    /// is shelled out against via `.current_dir(dir)` rather than the
    /// ambient cwd — see [`GhCli::pr_review_threads`]'s doc comment.
    fn resolve_repo(&self, dir: &Path) -> Result<RepoRef, GhError> {
        let output = Command::new("gh")
            .args(["repo", "view", "--json", "owner,name"])
            .current_dir(dir)
            .output()
            .map_err(|err| GhError::Spawn {
                command: "gh repo view".to_string(),
                message: err.to_string(),
            })?;

        interpret_repo_view_output(
            output.status.code(),
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        )
    }
}

/// Split an `"owner/name"` repo slug (e.g. `[backend].repo` from config)
/// into its `(owner, name)` parts, for methods that need to pass them as
/// separate `gh api graphql` variables rather than a single `-R` flag (see
/// [`GhCli::issue_dependencies`]).
///
/// A malformed slug (missing or extra `/`, either half empty) is a
/// [`GhError::Parse`] naming the offending string — this is a `tm`-side
/// configuration/caller bug, not something `gh` itself ever reports, so
/// there is no real stderr to attribute it to.
fn split_repo_slug(repo: &str) -> Result<(&str, &str), GhError> {
    match repo.split_once('/') {
        Some((owner, name)) if !owner.is_empty() && !name.is_empty() && !name.contains('/') => {
            Ok((owner, name))
        }
        _ => Err(GhError::Parse {
            command: "gh api graphql".to_string(),
            message: format!("expected an \"owner/name\" repo slug, got {repo:?}"),
        }),
    }
}

impl GhCli for ShellGhCli {
    fn pr_view(&self) -> Result<Option<PrInfo>, GhError> {
        let output = Command::new("gh")
            .args(["pr", "view", "--json", PR_VIEW_JSON_FIELDS])
            .output()
            .map_err(|err| GhError::Spawn {
                command: "gh pr view".to_string(),
                message: err.to_string(),
            })?;

        interpret_pr_view_output(
            output.status.code(),
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        )
    }

    fn pr_create(&self, req: &PrCreateRequest) -> Result<PrInfo, GhError> {
        let output = Command::new("gh")
            .args(pr_create_args(req))
            .output()
            .map_err(|err| GhError::Spawn {
                command: "gh pr create".to_string(),
                message: err.to_string(),
            })?;

        interpret_pr_create_output(
            output.status.code(),
            &String::from_utf8_lossy(&output.stderr),
        )?;

        self.pr_view()?.ok_or_else(|| GhError::Command {
            command: "gh pr view".to_string(),
            exit_code: None,
            stderr: "gh pr create succeeded but no PR was found for the current branch".to_string(),
        })
    }

    fn pr_edit(&self, number: u64, req: &PrEditRequest) -> Result<(), GhError> {
        let output = Command::new("gh")
            .args(pr_edit_args(number, req))
            .output()
            .map_err(|err| GhError::Spawn {
                command: "gh pr edit".to_string(),
                message: err.to_string(),
            })?;

        interpret_pr_edit_output(
            output.status.code(),
            &String::from_utf8_lossy(&output.stderr),
        )
    }

    fn pr_comment(&self, number: u64, body: &str) -> Result<(), GhError> {
        let output = Command::new("gh")
            .args(["pr", "comment", &number.to_string(), "--body", body])
            .output()
            .map_err(|err| GhError::Spawn {
                command: "gh pr comment".to_string(),
                message: err.to_string(),
            })?;

        interpret_pr_comment_output(
            output.status.code(),
            &String::from_utf8_lossy(&output.stderr),
        )
    }

    fn current_branch(&self) -> Result<String, GhError> {
        let output = Command::new("git")
            .args(["branch", "--show-current"])
            .output()
            .map_err(|err| GhError::Spawn {
                command: "git branch --show-current".to_string(),
                message: err.to_string(),
            })?;

        interpret_current_branch_output(
            output.status.code(),
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        )
    }

    fn pr_review_threads(&self, dir: &Path, number: u64) -> Result<Vec<ReviewThread>, GhError> {
        let repo = self.resolve_repo(dir)?;

        let query_arg = format!("query={REVIEW_THREADS_QUERY}");
        let owner_arg = format!("owner={}", repo.owner);
        let name_arg = format!("name={}", repo.name);
        let number_arg = format!("number={number}");
        let output = Command::new("gh")
            .args([
                "api",
                "graphql",
                "-f",
                &query_arg,
                "-F",
                &owner_arg,
                "-F",
                &name_arg,
                "-F",
                &number_arg,
            ])
            .current_dir(dir)
            .output()
            .map_err(|err| GhError::Spawn {
                command: "gh api graphql".to_string(),
                message: err.to_string(),
            })?;

        interpret_review_threads_output(
            output.status.code(),
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
            number,
        )
    }

    fn pr_reviews(&self, dir: &Path, number: u64) -> Result<Vec<PrReview>, GhError> {
        let repo = self.resolve_repo(dir)?;

        let path = format!("repos/{}/{}/pulls/{number}/reviews", repo.owner, repo.name);
        let output = Command::new("gh")
            .args(["api", &path])
            .current_dir(dir)
            .output()
            .map_err(|err| GhError::Spawn {
                command: "gh api pulls/reviews".to_string(),
                message: err.to_string(),
            })?;

        interpret_pr_reviews_output(
            output.status.code(),
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        )
    }

    fn pr_state(&self, dir: &Path, number: u64) -> Result<PrLifecycle, GhError> {
        let output = Command::new("gh")
            .args([
                "pr",
                "view",
                &number.to_string(),
                "--json",
                PR_STATE_JSON_FIELDS,
            ])
            .current_dir(dir)
            .output()
            .map_err(|err| GhError::Spawn {
                command: "gh pr view".to_string(),
                message: err.to_string(),
            })?;

        interpret_pr_state_output(
            output.status.code(),
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        )
    }

    fn pr_bot_finding_details(
        &self,
        dir: &Path,
        number: u64,
    ) -> Result<Vec<FindingDetail>, GhError> {
        let repo = self.resolve_repo(dir)?;

        let query_arg = format!("query={FINDING_DETAILS_QUERY}");
        let owner_arg = format!("owner={}", repo.owner);
        let name_arg = format!("name={}", repo.name);
        let number_arg = format!("number={number}");
        let output = Command::new("gh")
            .args([
                "api",
                "graphql",
                "-f",
                &query_arg,
                "-F",
                &owner_arg,
                "-F",
                &name_arg,
                "-F",
                &number_arg,
            ])
            .current_dir(dir)
            .output()
            .map_err(|err| GhError::Spawn {
                command: "gh api graphql".to_string(),
                message: err.to_string(),
            })?;

        interpret_finding_details_output(
            output.status.code(),
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
            number,
        )
    }

    fn pr_list(&self, dir: &Path) -> Result<Vec<PrInfo>, GhError> {
        let output = Command::new("gh")
            .args([
                "pr",
                "list",
                "--state",
                "open",
                "--limit",
                "200",
                "--json",
                PR_VIEW_JSON_FIELDS,
            ])
            .current_dir(dir)
            .output()
            .map_err(|err| GhError::Spawn {
                command: "gh pr list".to_string(),
                message: err.to_string(),
            })?;

        interpret_pr_list_output(
            output.status.code(),
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        )
    }

    fn pr_list_bounded(&self, dir: &Path, timeout: Duration) -> Result<Vec<PrInfo>, GhError> {
        let mut command = Command::new("gh");
        command
            .args([
                "pr",
                "list",
                "--state",
                "open",
                "--limit",
                "200",
                "--json",
                PR_VIEW_JSON_FIELDS,
            ])
            .current_dir(dir);

        let output = spawn_with_timeout(command, "gh pr list", timeout)?;

        interpret_pr_list_output(
            output.status.code(),
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        )
    }

    fn current_user_login(&self) -> Result<Option<String>, GhError> {
        let output = Command::new("gh")
            .args(["api", "user", "-q", ".login"])
            .output()
            .map_err(|err| GhError::Spawn {
                command: "gh api user".to_string(),
                message: err.to_string(),
            })?;

        Ok(interpret_current_user_login_output(
            output.status.code(),
            &String::from_utf8_lossy(&output.stdout),
        ))
    }

    fn pr_url_for_branch(&self, branch: &str) -> Result<Option<String>, GhError> {
        let output = Command::new("gh")
            .args(["pr", "list", "--head", branch, "--json", "url"])
            .output()
            .map_err(|err| GhError::Spawn {
                command: "gh pr list".to_string(),
                message: err.to_string(),
            })?;

        Ok(interpret_pr_url_for_branch_output(
            output.status.code(),
            &String::from_utf8_lossy(&output.stdout),
        ))
    }

    fn pr_list_all(&self, dir: &Path) -> Result<Vec<PrSummary>, GhError> {
        let output = Command::new("gh")
            .args([
                "pr",
                "list",
                "--state",
                "all",
                "--json",
                PR_LIST_ALL_JSON_FIELDS,
            ])
            .current_dir(dir)
            .output()
            .map_err(|err| GhError::Spawn {
                command: "gh pr list".to_string(),
                message: err.to_string(),
            })?;

        interpret_pr_list_all_output(
            output.status.code(),
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        )
    }

    fn issue_view(&self, repo: &str, number: u64) -> Result<IssueInfo, GhError> {
        let output = Command::new("gh")
            .args([
                "issue",
                "view",
                &number.to_string(),
                "-R",
                repo,
                "--json",
                ISSUE_JSON_FIELDS,
            ])
            .output()
            .map_err(|err| GhError::Spawn {
                command: "gh issue view".to_string(),
                message: err.to_string(),
            })?;

        interpret_issue_view_output(
            "gh issue view",
            output.status.code(),
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        )
    }

    fn issue_list(&self, repo: &str, filter: &IssueListFilter) -> Result<Vec<IssueInfo>, GhError> {
        let output = Command::new("gh")
            .args(issue_list_args(repo, filter))
            .output()
            .map_err(|err| GhError::Spawn {
                command: "gh issue list".to_string(),
                message: err.to_string(),
            })?;

        interpret_issue_list_output(
            output.status.code(),
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        )
    }

    fn issue_create(&self, repo: &str, req: &IssueCreateRequest) -> Result<IssueInfo, GhError> {
        let output = Command::new("gh")
            .args(issue_create_args(repo, req))
            .output()
            .map_err(|err| GhError::Spawn {
                command: "gh issue create".to_string(),
                message: err.to_string(),
            })?;

        let number = interpret_issue_create_output(
            output.status.code(),
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        )?;

        self.issue_view(repo, number)
    }

    fn issue_edit(&self, repo: &str, number: u64, req: &IssueEditRequest) -> Result<(), GhError> {
        if let Some(args) = issue_edit_args(repo, number, req) {
            let output = Command::new("gh")
                .args(args)
                .output()
                .map_err(|err| GhError::Spawn {
                    command: "gh issue edit".to_string(),
                    message: err.to_string(),
                })?;

            interpret_success_or_command_error(
                "gh issue edit",
                output.status.code(),
                &String::from_utf8_lossy(&output.stderr),
            )?;
        }

        if let Some(state) = req.state {
            let subcommand = match state {
                IssueStateChange::Close => "close",
                IssueStateChange::Reopen => "reopen",
            };
            let output = Command::new("gh")
                .args(["issue", subcommand, &number.to_string(), "-R", repo])
                .output()
                .map_err(|err| GhError::Spawn {
                    command: format!("gh issue {subcommand}"),
                    message: err.to_string(),
                })?;

            interpret_success_or_command_error(
                &format!("gh issue {subcommand}"),
                output.status.code(),
                &String::from_utf8_lossy(&output.stderr),
            )?;
        }

        Ok(())
    }

    fn issue_comment(&self, repo: &str, number: u64, body: &str) -> Result<(), GhError> {
        let output = Command::new("gh")
            .args([
                "issue",
                "comment",
                &number.to_string(),
                "-R",
                repo,
                "--body",
                body,
            ])
            .output()
            .map_err(|err| GhError::Spawn {
                command: "gh issue comment".to_string(),
                message: err.to_string(),
            })?;

        interpret_success_or_command_error(
            "gh issue comment",
            output.status.code(),
            &String::from_utf8_lossy(&output.stderr),
        )
    }

    fn issue_dependencies(&self, repo: &str, number: u64) -> Result<IssueDependencies, GhError> {
        let (owner, name) = split_repo_slug(repo)?;

        let query_arg = format!("query={ISSUE_DEPENDENCIES_QUERY}");
        let owner_arg = format!("owner={owner}");
        let name_arg = format!("name={name}");
        let number_arg = format!("number={number}");
        let output = Command::new("gh")
            .args([
                "api",
                "graphql",
                "-f",
                &query_arg,
                "-F",
                &owner_arg,
                "-F",
                &name_arg,
                "-F",
                &number_arg,
            ])
            .output()
            .map_err(|err| GhError::Spawn {
                command: "gh api graphql".to_string(),
                message: err.to_string(),
            })?;

        interpret_issue_dependencies_output(
            output.status.code(),
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
            number,
        )
    }
}

/// Interpret `gh api user -q .login` output: a non-zero exit or empty
/// stdout is tolerated as "no login resolved" rather than an error — see
/// [`GhCli::current_user_login`]'s doc comment.
fn interpret_current_user_login_output(exit_code: Option<i32>, stdout: &str) -> Option<String> {
    if exit_code != Some(0) {
        return None;
    }
    let login = stdout.trim();
    if login.is_empty() {
        None
    } else {
        Some(login.to_string())
    }
}

/// Raw shape of one entry in `gh pr list --head <branch> --json url` output,
/// for deserialization only.
#[derive(Debug, Deserialize)]
struct RawPrListUrlEntry {
    url: String,
}

/// Interpret the result of a `gh pr list --head <branch> --json url`
/// invocation. Tolerant by design (see [`GhCli::pr_url_for_branch`]): any
/// non-zero exit, unparseable output, or empty array is `None` rather than
/// an error — a missing/absent PR for a branch is the normal case, not a
/// failure.
fn interpret_pr_url_for_branch_output(exit_code: Option<i32>, stdout: &str) -> Option<String> {
    if exit_code != Some(0) {
        return None;
    }
    let entries = serde_json::from_str::<Vec<RawPrListUrlEntry>>(stdout).ok()?;
    entries.into_iter().next().map(|entry| entry.url)
}

/// An `owner`/`name` repository reference, as returned by
/// `gh repo view --json owner,name`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RepoRef {
    owner: String,
    name: String,
}

/// Raw shape of `gh repo view --json owner,name` output, for deserialization
/// only; [`RepoRef`] is the flattened shape callers use.
#[derive(Debug, Deserialize)]
struct RawRepoView {
    owner: RawRepoOwner,
    name: String,
}

#[derive(Debug, Deserialize)]
struct RawRepoOwner {
    login: String,
}

/// Interpret the result of a `gh repo view --json owner,name` invocation.
///
/// Pure over the exit code and captured stdout/stderr, for the same
/// testability reasons as [`interpret_pr_view_output`].
fn interpret_repo_view_output(
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
) -> Result<RepoRef, GhError> {
    match exit_code {
        Some(0) => serde_json::from_str::<RawRepoView>(stdout)
            .map(|raw| RepoRef {
                owner: raw.owner.login,
                name: raw.name,
            })
            .map_err(|err| GhError::Parse {
                command: "gh repo view".to_string(),
                message: err.to_string(),
            }),
        Some(code) => Err(GhError::Command {
            command: "gh repo view".to_string(),
            exit_code: Some(code),
            stderr: stderr.trim().to_string(),
        }),
        None => Err(GhError::Command {
            command: "gh repo view".to_string(),
            exit_code: None,
            stderr: stderr.trim().to_string(),
        }),
    }
}

/// Raw shape of the `gh api graphql` response body for
/// [`REVIEW_THREADS_QUERY`], for deserialization only.
#[derive(Debug, Deserialize)]
struct RawGraphQlResponse {
    data: Option<RawGraphQlData>,
}

#[derive(Debug, Deserialize)]
struct RawGraphQlData {
    repository: Option<RawGraphQlRepository>,
}

#[derive(Debug, Deserialize)]
struct RawGraphQlRepository {
    #[serde(rename = "pullRequest")]
    pull_request: Option<RawGraphQlPullRequest>,
}

#[derive(Debug, Deserialize)]
struct RawGraphQlPullRequest {
    #[serde(rename = "reviewThreads")]
    review_threads: RawGraphQlReviewThreads,
}

#[derive(Debug, Deserialize)]
struct RawGraphQlReviewThreads {
    nodes: Vec<RawGraphQlReviewThreadNode>,
}

#[derive(Debug, Deserialize)]
struct RawGraphQlReviewThreadNode {
    #[serde(rename = "isResolved")]
    is_resolved: bool,
    comments: RawGraphQlComments,
}

#[derive(Debug, Deserialize)]
struct RawGraphQlComments {
    nodes: Vec<RawGraphQlComment>,
}

#[derive(Debug, Deserialize)]
struct RawGraphQlComment {
    author: Option<RawGraphQlAuthor>,
}

#[derive(Debug, Deserialize)]
struct RawGraphQlAuthor {
    login: String,
}

/// Interpret the result of a `gh api graphql ...` invocation running
/// [`REVIEW_THREADS_QUERY`] for pull request `number`.
///
/// Pure over the exit code and captured stdout/stderr, for the same
/// testability reasons as [`interpret_pr_view_output`]. A null
/// `data.repository.pullRequest` (pull request not found) is treated as a
/// [`GhError::Parse`] naming `number`.
fn interpret_review_threads_output(
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
    number: u64,
) -> Result<Vec<ReviewThread>, GhError> {
    match exit_code {
        Some(0) => {
            let response = serde_json::from_str::<RawGraphQlResponse>(stdout).map_err(|err| {
                GhError::Parse {
                    command: "gh api graphql".to_string(),
                    message: err.to_string(),
                }
            })?;

            let pull_request = response
                .data
                .and_then(|data| data.repository)
                .and_then(|repo| repo.pull_request)
                .ok_or_else(|| GhError::Parse {
                    command: "gh api graphql".to_string(),
                    message: format!("pull request #{number} not found"),
                })?;

            Ok(pull_request
                .review_threads
                .nodes
                .into_iter()
                .map(|node| ReviewThread {
                    is_resolved: node.is_resolved,
                    author_login: node
                        .comments
                        .nodes
                        .into_iter()
                        .next()
                        .and_then(|comment| comment.author)
                        .map(|author| author.login),
                })
                .collect())
        }
        Some(code) => Err(GhError::Command {
            command: "gh api graphql".to_string(),
            exit_code: Some(code),
            stderr: stderr.trim().to_string(),
        }),
        None => Err(GhError::Command {
            command: "gh api graphql".to_string(),
            exit_code: None,
            stderr: stderr.trim().to_string(),
        }),
    }
}

/// Raw shape of the `gh api graphql` response body for
/// [`FINDING_DETAILS_QUERY`], for deserialization only. Deliberately not
/// shared with [`RawGraphQlResponse`]'s types (see
/// [`GhCli::pr_bot_finding_details`]).
#[derive(Debug, Deserialize)]
struct RawFindingGraphQlResponse {
    data: Option<RawFindingGraphQlData>,
}

#[derive(Debug, Deserialize)]
struct RawFindingGraphQlData {
    repository: Option<RawFindingGraphQlRepository>,
}

#[derive(Debug, Deserialize)]
struct RawFindingGraphQlRepository {
    #[serde(rename = "pullRequest")]
    pull_request: Option<RawFindingGraphQlPullRequest>,
}

#[derive(Debug, Deserialize)]
struct RawFindingGraphQlPullRequest {
    #[serde(rename = "reviewThreads")]
    review_threads: RawFindingGraphQlReviewThreads,
}

#[derive(Debug, Deserialize)]
struct RawFindingGraphQlReviewThreads {
    nodes: Vec<RawFindingGraphQlReviewThreadNode>,
}

#[derive(Debug, Deserialize)]
struct RawFindingGraphQlReviewThreadNode {
    #[serde(rename = "isResolved")]
    is_resolved: bool,
    comments: RawFindingGraphQlComments,
}

#[derive(Debug, Deserialize)]
struct RawFindingGraphQlComments {
    nodes: Vec<RawFindingGraphQlComment>,
}

#[derive(Debug, Deserialize)]
struct RawFindingGraphQlComment {
    author: Option<RawGraphQlAuthor>,
    body: String,
    path: Option<String>,
    line: Option<i64>,
    url: String,
}

/// Interpret the result of a `gh api graphql ...` invocation running
/// [`FINDING_DETAILS_QUERY`] for pull request `number`.
///
/// Pure over the exit code and captured stdout/stderr, for the same
/// testability reasons as [`interpret_pr_view_output`]. A review thread with
/// no first comment (shouldn't happen in practice — a thread always starts
/// with a comment) degrades to empty `body`/`url` and `None` `path`/`line`/
/// `author_login` rather than erroring. A null `data.repository.pullRequest`
/// (pull request not found) is a [`GhError::Parse`] naming `number`, same as
/// [`interpret_review_threads_output`].
fn interpret_finding_details_output(
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
    number: u64,
) -> Result<Vec<FindingDetail>, GhError> {
    match exit_code {
        Some(0) => {
            let response =
                serde_json::from_str::<RawFindingGraphQlResponse>(stdout).map_err(|err| {
                    GhError::Parse {
                        command: "gh api graphql".to_string(),
                        message: err.to_string(),
                    }
                })?;

            let pull_request = response
                .data
                .and_then(|data| data.repository)
                .and_then(|repo| repo.pull_request)
                .ok_or_else(|| GhError::Parse {
                    command: "gh api graphql".to_string(),
                    message: format!("pull request #{number} not found"),
                })?;

            Ok(pull_request
                .review_threads
                .nodes
                .into_iter()
                .map(|node| {
                    let comment = node.comments.nodes.into_iter().next();
                    FindingDetail {
                        author_login: comment
                            .as_ref()
                            .and_then(|comment| comment.author.as_ref())
                            .map(|author| author.login.clone()),
                        is_resolved: node.is_resolved,
                        path: comment.as_ref().and_then(|comment| comment.path.clone()),
                        line: comment.as_ref().and_then(|comment| comment.line),
                        body: comment
                            .as_ref()
                            .map(|comment| comment.body.clone())
                            .unwrap_or_default(),
                        url: comment.map(|comment| comment.url).unwrap_or_default(),
                    }
                })
                .collect())
        }
        Some(code) => Err(GhError::Command {
            command: "gh api graphql".to_string(),
            exit_code: Some(code),
            stderr: stderr.trim().to_string(),
        }),
        None => Err(GhError::Command {
            command: "gh api graphql".to_string(),
            exit_code: None,
            stderr: stderr.trim().to_string(),
        }),
    }
}

/// Raw shape of one entry in `gh api repos/{owner}/{repo}/pulls/{number}/reviews`
/// output, for deserialization only.
#[derive(Debug, Deserialize)]
struct RawPrReviewEntry {
    user: Option<RawPrReviewUser>,
}

#[derive(Debug, Deserialize)]
struct RawPrReviewUser {
    login: String,
}

/// Interpret the result of a `gh api repos/{owner}/{repo}/pulls/{number}/reviews`
/// invocation.
///
/// Pure over the exit code and captured stdout/stderr, for the same
/// testability reasons as [`interpret_pr_view_output`].
fn interpret_pr_reviews_output(
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
) -> Result<Vec<PrReview>, GhError> {
    match exit_code {
        Some(0) => serde_json::from_str::<Vec<RawPrReviewEntry>>(stdout)
            .map(|entries| {
                entries
                    .into_iter()
                    .map(|entry| PrReview {
                        author_login: entry.user.map(|user| user.login),
                    })
                    .collect()
            })
            .map_err(|err| GhError::Parse {
                command: "gh api pulls/reviews".to_string(),
                message: err.to_string(),
            }),
        Some(code) => Err(GhError::Command {
            command: "gh api pulls/reviews".to_string(),
            exit_code: Some(code),
            stderr: stderr.trim().to_string(),
        }),
        None => Err(GhError::Command {
            command: "gh api pulls/reviews".to_string(),
            exit_code: None,
            stderr: stderr.trim().to_string(),
        }),
    }
}

/// Raw shape of `gh pr view --json state` output, for deserialization only.
#[derive(Debug, Deserialize)]
struct RawPrState {
    state: String,
}

/// Interpret the result of a `gh pr view <number> --json state` invocation.
///
/// Pure over the exit code and captured stdout/stderr, for the same
/// testability reasons as [`interpret_pr_view_output`]. `gh` reports `state`
/// as `OPEN`, `CLOSED`, or `MERGED` directly (see [`PrLifecycle`]'s doc
/// comment), so no other field is needed to classify it.
fn interpret_pr_state_output(
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
) -> Result<PrLifecycle, GhError> {
    match exit_code {
        Some(0) => {
            let raw = serde_json::from_str::<RawPrState>(stdout).map_err(|err| GhError::Parse {
                command: "gh pr view".to_string(),
                message: err.to_string(),
            })?;
            Ok(if raw.state.eq_ignore_ascii_case("merged") {
                PrLifecycle::Merged
            } else if raw.state.eq_ignore_ascii_case("closed") {
                PrLifecycle::Closed
            } else {
                PrLifecycle::Open
            })
        }
        Some(code) => Err(GhError::Command {
            command: "gh pr view".to_string(),
            exit_code: Some(code),
            stderr: stderr.trim().to_string(),
        }),
        None => Err(GhError::Command {
            command: "gh pr view".to_string(),
            exit_code: None,
            stderr: stderr.trim().to_string(),
        }),
    }
}

/// Interpret the result of a `gh pr list --state open --json
/// number,url,title,body,headRefName` invocation.
///
/// Pure over the exit code and captured stdout/stderr, for the same
/// testability reasons as [`interpret_pr_view_output`]. Shares [`PrInfo`]'s
/// deserialization with [`interpret_pr_view_output`] since both commands
/// request the same field set ([`PR_VIEW_JSON_FIELDS`]).
fn interpret_pr_list_output(
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
) -> Result<Vec<PrInfo>, GhError> {
    match exit_code {
        Some(0) => serde_json::from_str::<Vec<PrInfo>>(stdout).map_err(|err| GhError::Parse {
            command: "gh pr list".to_string(),
            message: err.to_string(),
        }),
        Some(code) => Err(GhError::Command {
            command: "gh pr list".to_string(),
            exit_code: Some(code),
            stderr: stderr.trim().to_string(),
        }),
        None => Err(GhError::Command {
            command: "gh pr list".to_string(),
            exit_code: None,
            stderr: stderr.trim().to_string(),
        }),
    }
}

/// Raw shape of one entry in `gh pr list --state all --json
/// number,headRefName,state,updatedAt` output, for deserialization only;
/// [`PrSummary`] is the flattened shape callers use.
#[derive(Debug, Deserialize)]
struct RawPrListAllEntry {
    number: u64,
    #[serde(rename = "headRefName")]
    head_ref_name: String,
    state: String,
    #[serde(rename = "updatedAt")]
    updated_at: String,
}

/// Interpret the result of a `gh pr list --state all --json
/// number,headRefName,state,updatedAt` invocation.
///
/// Pure over the exit code and captured stdout/stderr, for the same
/// testability reasons as [`interpret_pr_view_output`]. `gh` reports `state`
/// as `OPEN`, `CLOSED`, or `MERGED` directly, same as
/// [`interpret_pr_state_output`], so no other field is needed to classify
/// [`PrLifecycle`].
fn interpret_pr_list_all_output(
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
) -> Result<Vec<PrSummary>, GhError> {
    match exit_code {
        Some(0) => serde_json::from_str::<Vec<RawPrListAllEntry>>(stdout)
            .map(|entries| {
                entries
                    .into_iter()
                    .map(|entry| PrSummary {
                        number: entry.number,
                        head_ref_name: entry.head_ref_name,
                        lifecycle: if entry.state.eq_ignore_ascii_case("merged") {
                            PrLifecycle::Merged
                        } else if entry.state.eq_ignore_ascii_case("closed") {
                            PrLifecycle::Closed
                        } else {
                            PrLifecycle::Open
                        },
                        updated_at: entry.updated_at,
                    })
                    .collect()
            })
            .map_err(|err| GhError::Parse {
                command: "gh pr list".to_string(),
                message: err.to_string(),
            }),
        Some(code) => Err(GhError::Command {
            command: "gh pr list".to_string(),
            exit_code: Some(code),
            stderr: stderr.trim().to_string(),
        }),
        None => Err(GhError::Command {
            command: "gh pr list".to_string(),
            exit_code: None,
            stderr: stderr.trim().to_string(),
        }),
    }
}

/// Interpret the result of a `gh pr view --json ...` invocation.
///
/// Pure over the exit code and captured stdout/stderr so parsing can be unit
/// tested without shelling out. `gh` exits non-zero with a "no pull requests
/// found" style message on stderr when the current branch has no PR; that
/// case is mapped to `Ok(None)` rather than an error. Any other non-zero
/// exit, or output that doesn't parse as [`PrInfo`], is an error.
fn interpret_pr_view_output(
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
) -> Result<Option<PrInfo>, GhError> {
    match exit_code {
        Some(0) => serde_json::from_str::<PrInfo>(stdout)
            .map(Some)
            .map_err(|err| GhError::Parse {
                command: "gh pr view".to_string(),
                message: err.to_string(),
            }),
        Some(_) if no_pr_found(stderr) => Ok(None),
        Some(code) => Err(GhError::Command {
            command: "gh pr view".to_string(),
            exit_code: Some(code),
            stderr: stderr.trim().to_string(),
        }),
        None => Err(GhError::Command {
            command: "gh pr view".to_string(),
            exit_code: None,
            stderr: stderr.trim().to_string(),
        }),
    }
}

/// Whether `gh`'s stderr indicates there is no pull request for the current
/// branch.
fn no_pr_found(stderr: &str) -> bool {
    stderr.to_lowercase().contains("no pull requests found")
}

/// Build the argument list for `gh pr create --title ... --body ... [--base ...]`.
fn pr_create_args(req: &PrCreateRequest) -> Vec<String> {
    let mut args = vec![
        "pr".to_string(),
        "create".to_string(),
        "--title".to_string(),
        req.title.clone(),
        "--body".to_string(),
        req.body.clone(),
    ];
    if let Some(base) = &req.base {
        args.push("--base".to_string());
        args.push(base.clone());
    }
    args
}

/// Interpret the result of a `gh pr create ...` invocation.
///
/// Pure over the exit code and captured stderr; the created PR's fields are
/// not parsed from this command's output, so only success/failure matters
/// here.
fn interpret_pr_create_output(exit_code: Option<i32>, stderr: &str) -> Result<(), GhError> {
    interpret_success_or_command_error("gh pr create", exit_code, stderr)
}

/// Build the argument list for `gh pr edit <number> [--title ...] [--body ...]`.
fn pr_edit_args(number: u64, req: &PrEditRequest) -> Vec<String> {
    let mut args = vec!["pr".to_string(), "edit".to_string(), number.to_string()];
    if let Some(title) = &req.title {
        args.push("--title".to_string());
        args.push(title.clone());
    }
    if let Some(body) = &req.body {
        args.push("--body".to_string());
        args.push(body.clone());
    }
    args
}

/// Interpret the result of a `gh pr edit ...` invocation.
///
/// Pure over the exit code and captured stderr, for the same reasons as
/// [`interpret_pr_create_output`].
fn interpret_pr_edit_output(exit_code: Option<i32>, stderr: &str) -> Result<(), GhError> {
    interpret_success_or_command_error("gh pr edit", exit_code, stderr)
}

/// Interpret the result of a `gh pr comment ...` invocation.
///
/// Pure over the exit code and captured stderr, for the same reasons as
/// [`interpret_pr_create_output`].
fn interpret_pr_comment_output(exit_code: Option<i32>, stderr: &str) -> Result<(), GhError> {
    interpret_success_or_command_error("gh pr comment", exit_code, stderr)
}

/// Shared success/failure interpretation for commands whose output carries
/// no information beyond "it worked": exit 0 is `Ok(())`, anything else is a
/// [`GhError::Command`] tagged with `command`.
fn interpret_success_or_command_error(
    command: &str,
    exit_code: Option<i32>,
    stderr: &str,
) -> Result<(), GhError> {
    match exit_code {
        Some(0) => Ok(()),
        Some(code) => Err(GhError::Command {
            command: command.to_string(),
            exit_code: Some(code),
            stderr: stderr.trim().to_string(),
        }),
        None => Err(GhError::Command {
            command: command.to_string(),
            exit_code: None,
            stderr: stderr.trim().to_string(),
        }),
    }
}

/// Interpret the result of a `git branch --show-current` invocation.
///
/// Pure over the exit code and captured stdout/stderr for the same
/// testability reasons as [`interpret_pr_view_output`].
fn interpret_current_branch_output(
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
) -> Result<String, GhError> {
    match exit_code {
        Some(0) => Ok(stdout.trim().to_string()),
        Some(code) => Err(GhError::Command {
            command: "git branch --show-current".to_string(),
            exit_code: Some(code),
            stderr: stderr.trim().to_string(),
        }),
        None => Err(GhError::Command {
            command: "git branch --show-current".to_string(),
            exit_code: None,
            stderr: stderr.trim().to_string(),
        }),
    }
}

/// Raw shape of one label entry in `gh issue view`/`gh issue list --json
/// labels` output, for deserialization only.
#[derive(Debug, Deserialize)]
struct RawIssueLabel {
    name: String,
}

/// Raw shape of one assignee entry in `gh issue view`/`gh issue list --json
/// assignees` output, for deserialization only.
#[derive(Debug, Deserialize)]
struct RawIssueAssignee {
    login: String,
}

/// Raw shape of `gh issue view`/one entry of `gh issue list --json
/// {ISSUE_JSON_FIELDS}` output, for deserialization only; [`IssueInfo`] is
/// the flattened shape callers use.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawIssueView {
    number: u64,
    url: String,
    title: String,
    body: String,
    state: String,
    labels: Vec<RawIssueLabel>,
    assignees: Vec<RawIssueAssignee>,
}

impl From<RawIssueView> for IssueInfo {
    fn from(raw: RawIssueView) -> Self {
        IssueInfo {
            number: raw.number,
            url: raw.url,
            title: raw.title,
            body: raw.body,
            state: if raw.state.eq_ignore_ascii_case("closed") {
                IssueState::Closed
            } else {
                IssueState::Open
            },
            labels: raw.labels.into_iter().map(|label| label.name).collect(),
            assignees: raw
                .assignees
                .into_iter()
                .map(|assignee| assignee.login)
                .collect(),
        }
    }
}

/// Interpret the result of a `gh issue view <number> -R <repo> --json
/// {ISSUE_JSON_FIELDS}` invocation.
///
/// Pure over the exit code and captured stdout/stderr, for the same
/// testability reasons as [`interpret_pr_view_output`]. Unlike
/// [`GhCli::pr_view`], there is no "not found" tolerance here: a missing
/// issue number is always an error, since (unlike a branch's PR) an issue
/// number is always supplied explicitly by the caller and "not found" is
/// exactly as much a caller mistake as any other non-zero exit.
fn interpret_issue_view_output(
    command: &str,
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
) -> Result<IssueInfo, GhError> {
    match exit_code {
        Some(0) => serde_json::from_str::<RawIssueView>(stdout)
            .map(IssueInfo::from)
            .map_err(|err| GhError::Parse {
                command: command.to_string(),
                message: err.to_string(),
            }),
        Some(code) => Err(GhError::Command {
            command: command.to_string(),
            exit_code: Some(code),
            stderr: stderr.trim().to_string(),
        }),
        None => Err(GhError::Command {
            command: command.to_string(),
            exit_code: None,
            stderr: stderr.trim().to_string(),
        }),
    }
}

/// Build the argument list for `gh issue list -R <repo> --state ... --limit
/// ... [--label ...] [--assignee ...] --json {ISSUE_JSON_FIELDS}`.
fn issue_list_args(repo: &str, filter: &IssueListFilter) -> Vec<String> {
    let mut args = vec![
        "issue".to_string(),
        "list".to_string(),
        "-R".to_string(),
        repo.to_string(),
        "--state".to_string(),
        filter.state.as_gh_arg().to_string(),
        "--limit".to_string(),
        filter.limit.to_string(),
        "--json".to_string(),
        ISSUE_JSON_FIELDS.to_string(),
    ];
    if !filter.labels.is_empty() {
        args.push("--label".to_string());
        args.push(filter.labels.join(","));
    }
    if let Some(assignee) = &filter.assignee {
        args.push("--assignee".to_string());
        args.push(assignee.clone());
    }
    args
}

/// Interpret the result of a `gh issue list ...` invocation.
///
/// Pure over the exit code and captured stdout/stderr, for the same
/// testability reasons as [`interpret_pr_view_output`].
fn interpret_issue_list_output(
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
) -> Result<Vec<IssueInfo>, GhError> {
    match exit_code {
        Some(0) => serde_json::from_str::<Vec<RawIssueView>>(stdout)
            .map(|entries| entries.into_iter().map(IssueInfo::from).collect())
            .map_err(|err| GhError::Parse {
                command: "gh issue list".to_string(),
                message: err.to_string(),
            }),
        Some(code) => Err(GhError::Command {
            command: "gh issue list".to_string(),
            exit_code: Some(code),
            stderr: stderr.trim().to_string(),
        }),
        None => Err(GhError::Command {
            command: "gh issue list".to_string(),
            exit_code: None,
            stderr: stderr.trim().to_string(),
        }),
    }
}

/// Build the argument list for `gh issue create -R <repo> --title ... --body
/// ... [--label ...] [--assignee ...]`.
fn issue_create_args(repo: &str, req: &IssueCreateRequest) -> Vec<String> {
    let mut args = vec![
        "issue".to_string(),
        "create".to_string(),
        "-R".to_string(),
        repo.to_string(),
        "--title".to_string(),
        req.title.clone(),
        "--body".to_string(),
        req.body.clone(),
    ];
    if !req.labels.is_empty() {
        args.push("--label".to_string());
        args.push(req.labels.join(","));
    }
    if !req.assignees.is_empty() {
        args.push("--assignee".to_string());
        args.push(req.assignees.join(","));
    }
    args
}

/// Extract an issue number from a `gh issue create`-printed URL
/// (`https://github.com/<owner>/<repo>/issues/<number>`).
///
/// Pure so it can be unit tested without shelling out. Returns `None` if the
/// last path segment isn't a valid number (a URL shape `gh` isn't expected
/// to ever actually produce, but this avoids a panic if it somehow did).
fn parse_issue_number_from_url(url: &str) -> Option<u64> {
    url.trim().rsplit('/').next()?.parse().ok()
}

/// Interpret the result of a `gh issue create ...` invocation, returning the
/// created issue's number.
///
/// Pure over the exit code and captured stdout/stderr, for the same
/// testability reasons as [`interpret_pr_view_output`]. Unlike [`GhCli::pr_create`]
/// (which re-resolves the PR via `pr_view` against the current branch),
/// there is no "current issue" to look up by ambient state, so the number is
/// parsed directly out of the URL `gh issue create` prints to stdout on
/// success (see [`parse_issue_number_from_url`]).
fn interpret_issue_create_output(
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
) -> Result<u64, GhError> {
    match exit_code {
        Some(0) => parse_issue_number_from_url(stdout).ok_or_else(|| GhError::Parse {
            command: "gh issue create".to_string(),
            message: format!("could not parse an issue number out of {stdout:?}"),
        }),
        Some(code) => Err(GhError::Command {
            command: "gh issue create".to_string(),
            exit_code: Some(code),
            stderr: stderr.trim().to_string(),
        }),
        None => Err(GhError::Command {
            command: "gh issue create".to_string(),
            exit_code: None,
            stderr: stderr.trim().to_string(),
        }),
    }
}

/// Build the argument list for `gh issue edit <number> -R <repo>
/// [--add-label ...] [--remove-label ...] [--add-assignee ...]
/// [--remove-assignee ...]`, or `None` if `req` carries no label/assignee
/// change at all (in which case [`GhCli::issue_edit`] skips this `gh` call
/// entirely — `gh issue edit` errors if given no flags, and a
/// state-change-only edit is a legitimate, common request. e.g. `Done`).
fn issue_edit_args(repo: &str, number: u64, req: &IssueEditRequest) -> Option<Vec<String>> {
    if req.add_labels.is_empty()
        && req.remove_labels.is_empty()
        && req.add_assignees.is_empty()
        && req.remove_assignees.is_empty()
    {
        return None;
    }

    let mut args = vec![
        "issue".to_string(),
        "edit".to_string(),
        number.to_string(),
        "-R".to_string(),
        repo.to_string(),
    ];
    if !req.add_labels.is_empty() {
        args.push("--add-label".to_string());
        args.push(req.add_labels.join(","));
    }
    if !req.remove_labels.is_empty() {
        args.push("--remove-label".to_string());
        args.push(req.remove_labels.join(","));
    }
    if !req.add_assignees.is_empty() {
        args.push("--add-assignee".to_string());
        args.push(req.add_assignees.join(","));
    }
    if !req.remove_assignees.is_empty() {
        args.push("--remove-assignee".to_string());
        args.push(req.remove_assignees.join(","));
    }
    Some(args)
}

/// Raw shape of one issue node (`{number, title, state, url}`) inside
/// [`ISSUE_DEPENDENCIES_QUERY`]'s response, for deserialization only.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDependencyIssueNode {
    number: u64,
    title: String,
    state: String,
    url: String,
}

impl From<RawDependencyIssueNode> for IssueRef {
    fn from(raw: RawDependencyIssueNode) -> Self {
        IssueRef {
            number: raw.number,
            title: raw.title,
            state: if raw.state.eq_ignore_ascii_case("closed") {
                IssueState::Closed
            } else {
                IssueState::Open
            },
            url: raw.url,
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawDependencyConnection {
    nodes: Vec<RawDependencyIssueNode>,
}

/// Raw shape of the `gh api graphql` response body for
/// [`ISSUE_DEPENDENCIES_QUERY`], for deserialization only.
#[derive(Debug, Deserialize)]
struct RawIssueDependenciesResponse {
    data: Option<RawIssueDependenciesData>,
}

#[derive(Debug, Deserialize)]
struct RawIssueDependenciesData {
    repository: Option<RawIssueDependenciesRepository>,
}

#[derive(Debug, Deserialize)]
struct RawIssueDependenciesRepository {
    issue: Option<RawIssueDependenciesIssue>,
}

#[derive(Debug, Deserialize)]
struct RawIssueDependenciesIssue {
    #[serde(rename = "blockedBy")]
    blocked_by: RawDependencyConnection,
    blocking: RawDependencyConnection,
}

/// Interpret the result of a `gh api graphql ...` invocation running
/// [`ISSUE_DEPENDENCIES_QUERY`] for issue `number`.
///
/// Pure over the exit code and captured stdout/stderr, for the same
/// testability reasons as [`interpret_pr_view_output`]. A null
/// `data.repository.issue` (issue not found) is a [`GhError::Parse`] naming
/// `number`, the same convention as [`interpret_review_threads_output`].
fn interpret_issue_dependencies_output(
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
    number: u64,
) -> Result<IssueDependencies, GhError> {
    match exit_code {
        Some(0) => {
            let response =
                serde_json::from_str::<RawIssueDependenciesResponse>(stdout).map_err(|err| {
                    GhError::Parse {
                        command: "gh api graphql".to_string(),
                        message: err.to_string(),
                    }
                })?;

            let issue = response
                .data
                .and_then(|data| data.repository)
                .and_then(|repo| repo.issue)
                .ok_or_else(|| GhError::Parse {
                    command: "gh api graphql".to_string(),
                    message: format!("issue #{number} not found"),
                })?;

            Ok(IssueDependencies {
                blocked_by: issue
                    .blocked_by
                    .nodes
                    .into_iter()
                    .map(IssueRef::from)
                    .collect(),
                blocking: issue
                    .blocking
                    .nodes
                    .into_iter()
                    .map(IssueRef::from)
                    .collect(),
            })
        }
        Some(code) => Err(GhError::Command {
            command: "gh api graphql".to_string(),
            exit_code: Some(code),
            stderr: stderr.trim().to_string(),
        }),
        None => Err(GhError::Command {
            command: "gh api graphql".to_string(),
            exit_code: None,
            stderr: stderr.trim().to_string(),
        }),
    }
}

/// A [`GhCli`] test double: returns canned results and records the
/// `pr_create`/`pr_edit` calls made against it, for use by tests (including
/// later ticketing-flow tests) that don't want to shell out to a real `gh`.
///
/// This is a plain public struct (not `#[cfg(test)]`-gated) so other test
/// code in the crate can depend on it directly.
pub struct FakeGhCli {
    pr_view_result: RefCell<Result<Option<PrInfo>, GhError>>,
    current_branch_result: RefCell<Result<String, GhError>>,
    pr_create_result: RefCell<Result<PrInfo, GhError>>,
    pr_edit_result: RefCell<Result<(), GhError>>,
    pr_comment_result: RefCell<Result<(), GhError>>,
    pr_create_calls: RefCell<Vec<PrCreateRequest>>,
    pr_edit_calls: RefCell<Vec<(u64, PrEditRequest)>>,
    pr_comment_calls: RefCell<Vec<(u64, String)>>,
    review_threads_results: RefCell<HashMap<u64, Result<Vec<ReviewThread>, GhError>>>,
    review_threads_calls: RefCell<Vec<u64>>,
    pr_reviews_results: RefCell<HashMap<u64, Result<Vec<PrReview>, GhError>>>,
    pr_reviews_calls: RefCell<Vec<u64>>,
    pr_state_results: RefCell<HashMap<u64, Result<PrLifecycle, GhError>>>,
    pr_state_calls: RefCell<Vec<u64>>,
    finding_details_results: RefCell<HashMap<u64, Result<Vec<FindingDetail>, GhError>>>,
    finding_details_calls: RefCell<Vec<u64>>,
    pr_list_result: RefCell<Result<Vec<PrInfo>, GhError>>,
    pr_list_calls: RefCell<Vec<PathBuf>>,
    pr_list_bounded_result: RefCell<Option<Result<Vec<PrInfo>, GhError>>>,
    pr_list_bounded_calls: RefCell<Vec<PathBuf>>,
    current_user_login_result: RefCell<Result<Option<String>, GhError>>,
    pr_url_for_branch_result: RefCell<Result<Option<String>, GhError>>,
    pr_url_for_branch_calls: RefCell<Vec<String>>,
    pr_list_all_result: RefCell<Result<Vec<PrSummary>, GhError>>,
    pr_list_all_calls: RefCell<Vec<PathBuf>>,
    issue_view_results: RefCell<HashMap<u64, Result<IssueInfo, GhError>>>,
    issue_view_calls: RefCell<Vec<(String, u64)>>,
    issue_list_result: RefCell<Result<Vec<IssueInfo>, GhError>>,
    issue_list_calls: RefCell<Vec<(String, IssueListFilter)>>,
    issue_create_result: RefCell<Result<IssueInfo, GhError>>,
    issue_create_calls: RefCell<Vec<(String, IssueCreateRequest)>>,
    issue_edit_result: RefCell<Result<(), GhError>>,
    issue_edit_calls: RefCell<Vec<(String, u64, IssueEditRequest)>>,
    issue_comment_result: RefCell<Result<(), GhError>>,
    issue_comment_calls: RefCell<Vec<(String, u64, String)>>,
    issue_dependencies_results: RefCell<HashMap<u64, Result<IssueDependencies, GhError>>>,
    issue_dependencies_calls: RefCell<Vec<(String, u64)>>,
}

impl Default for FakeGhCli {
    /// A fake with no PR for the current branch, on branch `main`, and
    /// `pr_create`/`pr_edit` succeeding trivially. Override with the
    /// `with_*` builders as needed.
    fn default() -> Self {
        Self {
            pr_view_result: RefCell::new(Ok(None)),
            current_branch_result: RefCell::new(Ok("main".to_string())),
            pr_create_result: RefCell::new(Ok(PrInfo {
                number: 0,
                url: String::new(),
                title: String::new(),
                body: String::new(),
                head_ref_name: String::new(),
            })),
            pr_edit_result: RefCell::new(Ok(())),
            pr_comment_result: RefCell::new(Ok(())),
            pr_create_calls: RefCell::new(Vec::new()),
            pr_edit_calls: RefCell::new(Vec::new()),
            pr_comment_calls: RefCell::new(Vec::new()),
            review_threads_results: RefCell::new(HashMap::new()),
            review_threads_calls: RefCell::new(Vec::new()),
            pr_reviews_results: RefCell::new(HashMap::new()),
            pr_reviews_calls: RefCell::new(Vec::new()),
            pr_state_results: RefCell::new(HashMap::new()),
            pr_state_calls: RefCell::new(Vec::new()),
            finding_details_results: RefCell::new(HashMap::new()),
            finding_details_calls: RefCell::new(Vec::new()),
            pr_list_result: RefCell::new(Ok(Vec::new())),
            pr_list_calls: RefCell::new(Vec::new()),
            pr_list_bounded_result: RefCell::new(None),
            pr_list_bounded_calls: RefCell::new(Vec::new()),
            current_user_login_result: RefCell::new(Ok(None)),
            pr_url_for_branch_result: RefCell::new(Ok(None)),
            pr_url_for_branch_calls: RefCell::new(Vec::new()),
            pr_list_all_result: RefCell::new(Ok(Vec::new())),
            pr_list_all_calls: RefCell::new(Vec::new()),
            issue_view_results: RefCell::new(HashMap::new()),
            issue_view_calls: RefCell::new(Vec::new()),
            issue_list_result: RefCell::new(Ok(Vec::new())),
            issue_list_calls: RefCell::new(Vec::new()),
            issue_create_result: RefCell::new(Ok(IssueInfo {
                number: 0,
                url: String::new(),
                title: String::new(),
                body: String::new(),
                state: IssueState::Open,
                labels: Vec::new(),
                assignees: Vec::new(),
            })),
            issue_create_calls: RefCell::new(Vec::new()),
            issue_edit_result: RefCell::new(Ok(())),
            issue_edit_calls: RefCell::new(Vec::new()),
            issue_comment_result: RefCell::new(Ok(())),
            issue_comment_calls: RefCell::new(Vec::new()),
            issue_dependencies_results: RefCell::new(HashMap::new()),
            issue_dependencies_calls: RefCell::new(Vec::new()),
        }
    }
}

impl FakeGhCli {
    /// Create a fake with the default canned results (see [`Default`]).
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the result `pr_view` will return.
    pub fn with_pr_view(self, result: Result<Option<PrInfo>, GhError>) -> Self {
        *self.pr_view_result.borrow_mut() = result;
        self
    }

    /// Set the result `current_branch` will return.
    pub fn with_current_branch(self, result: Result<String, GhError>) -> Self {
        *self.current_branch_result.borrow_mut() = result;
        self
    }

    /// Set the result `pr_create` will return.
    pub fn with_pr_create_result(self, result: Result<PrInfo, GhError>) -> Self {
        *self.pr_create_result.borrow_mut() = result;
        self
    }

    /// Set the result `pr_edit` will return.
    pub fn with_pr_edit_result(self, result: Result<(), GhError>) -> Self {
        *self.pr_edit_result.borrow_mut() = result;
        self
    }

    /// The requests passed to `pr_create`, in call order.
    pub fn pr_create_calls(&self) -> Vec<PrCreateRequest> {
        self.pr_create_calls.borrow().clone()
    }

    /// The `(number, request)` pairs passed to `pr_edit`, in call order.
    pub fn pr_edit_calls(&self) -> Vec<(u64, PrEditRequest)> {
        self.pr_edit_calls.borrow().clone()
    }

    /// Set the result `pr_comment` will return.
    pub fn with_pr_comment_result(self, result: Result<(), GhError>) -> Self {
        *self.pr_comment_result.borrow_mut() = result;
        self
    }

    /// The `(number, body)` pairs passed to `pr_comment`, in call order.
    pub fn pr_comment_calls(&self) -> Vec<(u64, String)> {
        self.pr_comment_calls.borrow().clone()
    }

    /// Set the result `pr_review_threads` will return for pull request
    /// `number`.
    ///
    /// A PR number with no configured result returns `Ok(vec![])`, mirroring
    /// how [`FakeGhCli::default`] treats unconfigured queries as trivially
    /// empty rather than erroring.
    pub fn with_review_threads(
        self,
        number: u64,
        result: Result<Vec<ReviewThread>, GhError>,
    ) -> Self {
        self.review_threads_results
            .borrow_mut()
            .insert(number, result);
        self
    }

    /// The pull request numbers passed to `pr_review_threads`, in call order.
    pub fn pr_review_threads_calls(&self) -> Vec<u64> {
        self.review_threads_calls.borrow().clone()
    }

    /// Set the result `pr_reviews` will return for pull request `number`.
    ///
    /// A PR number with no configured result returns `Ok(vec![])`, mirroring
    /// [`FakeGhCli::with_review_threads`]'s unconfigured-is-trivially-empty
    /// convention.
    pub fn with_pr_reviews(self, number: u64, result: Result<Vec<PrReview>, GhError>) -> Self {
        self.pr_reviews_results.borrow_mut().insert(number, result);
        self
    }

    /// The pull request numbers passed to `pr_reviews`, in call order.
    pub fn pr_reviews_calls(&self) -> Vec<u64> {
        self.pr_reviews_calls.borrow().clone()
    }

    /// Set the result `pr_state` will return for pull request `number`.
    ///
    /// A PR number with no configured result returns `Ok(PrLifecycle::Open)`
    /// — an unconfigured PR is trivially still open, the same "nothing
    /// interesting configured" default other Fake lookups use.
    pub fn with_pr_state(self, number: u64, result: Result<PrLifecycle, GhError>) -> Self {
        self.pr_state_results.borrow_mut().insert(number, result);
        self
    }

    /// The pull request numbers passed to `pr_state`, in call order.
    pub fn pr_state_calls(&self) -> Vec<u64> {
        self.pr_state_calls.borrow().clone()
    }

    /// Set the result `pr_bot_finding_details` will return for pull request
    /// `number`.
    ///
    /// A PR number with no configured result returns `Ok(vec![])`, mirroring
    /// [`FakeGhCli::with_review_threads`]'s unconfigured-is-trivially-empty
    /// convention.
    pub fn with_pr_bot_finding_details(
        self,
        number: u64,
        result: Result<Vec<FindingDetail>, GhError>,
    ) -> Self {
        self.finding_details_results
            .borrow_mut()
            .insert(number, result);
        self
    }

    /// The pull request numbers passed to `pr_bot_finding_details`, in call
    /// order.
    pub fn pr_bot_finding_details_calls(&self) -> Vec<u64> {
        self.finding_details_calls.borrow().clone()
    }

    /// Set the result `pr_list` will return.
    pub fn with_pr_list(self, result: Result<Vec<PrInfo>, GhError>) -> Self {
        *self.pr_list_result.borrow_mut() = result;
        self
    }

    /// The `dir` arguments passed to `pr_list`, in call order — lets a test
    /// assert the resolved repo root (not the test process's cwd) is what
    /// gets passed, same rationale as [`FakeGhCli::pr_list_all_calls`].
    pub fn pr_list_calls(&self) -> Vec<PathBuf> {
        self.pr_list_calls.borrow().clone()
    }

    /// Set the result `pr_list_bounded` will return, e.g.
    /// `Err(GhError::Timeout { .. })` to simulate the board's `o`-key lookup
    /// timing out. Unconfigured (the default), `pr_list_bounded` delegates to
    /// whatever `pr_list_result` is set to, mirroring [`FakeGhCli::pr_list`]'s
    /// success case for tests that don't care about the timeout path.
    pub fn with_pr_list_bounded(self, result: Result<Vec<PrInfo>, GhError>) -> Self {
        *self.pr_list_bounded_result.borrow_mut() = Some(result);
        self
    }

    /// The `dir` arguments passed to `pr_list_bounded`, in call order.
    pub fn pr_list_bounded_calls(&self) -> Vec<PathBuf> {
        self.pr_list_bounded_calls.borrow().clone()
    }

    /// Set the result `current_user_login` will return.
    pub fn with_current_user_login(self, result: Result<Option<String>, GhError>) -> Self {
        *self.current_user_login_result.borrow_mut() = result;
        self
    }

    /// Set the result `pr_url_for_branch` will return.
    pub fn with_pr_url_for_branch(self, result: Result<Option<String>, GhError>) -> Self {
        *self.pr_url_for_branch_result.borrow_mut() = result;
        self
    }

    /// The branches passed to `pr_url_for_branch`, in call order.
    pub fn pr_url_for_branch_calls(&self) -> Vec<String> {
        self.pr_url_for_branch_calls.borrow().clone()
    }

    /// Set the result `pr_list_all` will return.
    pub fn with_pr_list_all(self, result: Result<Vec<PrSummary>, GhError>) -> Self {
        *self.pr_list_all_result.borrow_mut() = result;
        self
    }

    /// The `dir` arguments passed to `pr_list_all`, in call order. The
    /// count doubles as "was `gh` consulted at all" for blocker-stacking
    /// tests asserting it's skipped (e.g. `--from` given, or no ticket); the
    /// values themselves let a test assert the repo root — not the test
    /// process's cwd — is what gets passed (see [`GhCli::pr_list_all`]'s
    /// doc comment on why that distinction matters).
    pub fn pr_list_all_calls(&self) -> Vec<PathBuf> {
        self.pr_list_all_calls.borrow().clone()
    }

    /// Set the result `issue_view` will return for issue `number`.
    ///
    /// An issue number with no configured result returns
    /// `Err(GhError::Command { .. })`, unlike this fake's other unconfigured
    /// lookups (which default to trivially empty) — an issue number is
    /// always caller-supplied, so silently returning a blank [`IssueInfo`]
    /// would hide a test's forgotten setup rather than a real "not found"
    /// case.
    pub fn with_issue_view(self, number: u64, result: Result<IssueInfo, GhError>) -> Self {
        self.issue_view_results.borrow_mut().insert(number, result);
        self
    }

    /// The `(repo, number)` pairs passed to `issue_view`, in call order.
    pub fn issue_view_calls(&self) -> Vec<(String, u64)> {
        self.issue_view_calls.borrow().clone()
    }

    /// Set the result `issue_list` will return.
    pub fn with_issue_list(self, result: Result<Vec<IssueInfo>, GhError>) -> Self {
        *self.issue_list_result.borrow_mut() = result;
        self
    }

    /// The `(repo, filter)` pairs passed to `issue_list`, in call order.
    pub fn issue_list_calls(&self) -> Vec<(String, IssueListFilter)> {
        self.issue_list_calls.borrow().clone()
    }

    /// Set the result `issue_create` will return.
    pub fn with_issue_create_result(self, result: Result<IssueInfo, GhError>) -> Self {
        *self.issue_create_result.borrow_mut() = result;
        self
    }

    /// The `(repo, request)` pairs passed to `issue_create`, in call order.
    pub fn issue_create_calls(&self) -> Vec<(String, IssueCreateRequest)> {
        self.issue_create_calls.borrow().clone()
    }

    /// Set the result `issue_edit` will return.
    pub fn with_issue_edit_result(self, result: Result<(), GhError>) -> Self {
        *self.issue_edit_result.borrow_mut() = result;
        self
    }

    /// The `(repo, number, request)` triples passed to `issue_edit`, in call
    /// order.
    pub fn issue_edit_calls(&self) -> Vec<(String, u64, IssueEditRequest)> {
        self.issue_edit_calls.borrow().clone()
    }

    /// Set the result `issue_comment` will return.
    pub fn with_issue_comment_result(self, result: Result<(), GhError>) -> Self {
        *self.issue_comment_result.borrow_mut() = result;
        self
    }

    /// The `(repo, number, body)` triples passed to `issue_comment`, in call
    /// order.
    pub fn issue_comment_calls(&self) -> Vec<(String, u64, String)> {
        self.issue_comment_calls.borrow().clone()
    }

    /// Set the result `issue_dependencies` will return for issue `number`.
    ///
    /// An issue number with no configured result returns
    /// `Ok(IssueDependencies::default())` (no dependencies either way),
    /// mirroring [`FakeGhCli::with_review_threads`]'s
    /// unconfigured-is-trivially-empty convention — unlike
    /// [`FakeGhCli::with_issue_view`], "no dependencies" is a perfectly
    /// normal, common result, not a sign of forgotten test setup.
    pub fn with_issue_dependencies(
        self,
        number: u64,
        result: Result<IssueDependencies, GhError>,
    ) -> Self {
        self.issue_dependencies_results
            .borrow_mut()
            .insert(number, result);
        self
    }

    /// The `(repo, number)` pairs passed to `issue_dependencies`, in call
    /// order.
    pub fn issue_dependencies_calls(&self) -> Vec<(String, u64)> {
        self.issue_dependencies_calls.borrow().clone()
    }
}

impl GhCli for FakeGhCli {
    fn pr_view(&self) -> Result<Option<PrInfo>, GhError> {
        self.pr_view_result.borrow().clone()
    }

    fn pr_create(&self, req: &PrCreateRequest) -> Result<PrInfo, GhError> {
        self.pr_create_calls.borrow_mut().push(req.clone());
        self.pr_create_result.borrow().clone()
    }

    fn pr_edit(&self, number: u64, req: &PrEditRequest) -> Result<(), GhError> {
        self.pr_edit_calls.borrow_mut().push((number, req.clone()));
        self.pr_edit_result.borrow().clone()
    }

    fn pr_comment(&self, number: u64, body: &str) -> Result<(), GhError> {
        self.pr_comment_calls
            .borrow_mut()
            .push((number, body.to_string()));
        self.pr_comment_result.borrow().clone()
    }

    fn current_branch(&self) -> Result<String, GhError> {
        self.current_branch_result.borrow().clone()
    }

    fn pr_review_threads(&self, _dir: &Path, number: u64) -> Result<Vec<ReviewThread>, GhError> {
        self.review_threads_calls.borrow_mut().push(number);
        match self.review_threads_results.borrow().get(&number) {
            Some(result) => result.clone(),
            None => Ok(Vec::new()),
        }
    }

    fn pr_reviews(&self, _dir: &Path, number: u64) -> Result<Vec<PrReview>, GhError> {
        self.pr_reviews_calls.borrow_mut().push(number);
        match self.pr_reviews_results.borrow().get(&number) {
            Some(result) => result.clone(),
            None => Ok(Vec::new()),
        }
    }

    fn pr_state(&self, _dir: &Path, number: u64) -> Result<PrLifecycle, GhError> {
        self.pr_state_calls.borrow_mut().push(number);
        match self.pr_state_results.borrow().get(&number) {
            Some(result) => result.clone(),
            None => Ok(PrLifecycle::Open),
        }
    }

    fn pr_bot_finding_details(
        &self,
        _dir: &Path,
        number: u64,
    ) -> Result<Vec<FindingDetail>, GhError> {
        self.finding_details_calls.borrow_mut().push(number);
        match self.finding_details_results.borrow().get(&number) {
            Some(result) => result.clone(),
            None => Ok(Vec::new()),
        }
    }

    fn pr_list(&self, dir: &Path) -> Result<Vec<PrInfo>, GhError> {
        self.pr_list_calls.borrow_mut().push(dir.to_path_buf());
        self.pr_list_result.borrow().clone()
    }

    fn pr_list_bounded(&self, dir: &Path, _timeout: Duration) -> Result<Vec<PrInfo>, GhError> {
        self.pr_list_bounded_calls
            .borrow_mut()
            .push(dir.to_path_buf());
        match self.pr_list_bounded_result.borrow().clone() {
            Some(result) => result,
            None => self.pr_list_result.borrow().clone(),
        }
    }

    fn current_user_login(&self) -> Result<Option<String>, GhError> {
        self.current_user_login_result.borrow().clone()
    }

    fn pr_url_for_branch(&self, branch: &str) -> Result<Option<String>, GhError> {
        self.pr_url_for_branch_calls
            .borrow_mut()
            .push(branch.to_string());
        self.pr_url_for_branch_result.borrow().clone()
    }

    fn pr_list_all(&self, dir: &Path) -> Result<Vec<PrSummary>, GhError> {
        self.pr_list_all_calls.borrow_mut().push(dir.to_path_buf());
        self.pr_list_all_result.borrow().clone()
    }

    fn issue_view(&self, repo: &str, number: u64) -> Result<IssueInfo, GhError> {
        self.issue_view_calls
            .borrow_mut()
            .push((repo.to_string(), number));
        match self.issue_view_results.borrow().get(&number) {
            Some(result) => result.clone(),
            None => Err(GhError::Command {
                command: "gh issue view".to_string(),
                exit_code: Some(1),
                stderr: format!("no FakeGhCli issue_view result configured for #{number}"),
            }),
        }
    }

    fn issue_list(&self, repo: &str, filter: &IssueListFilter) -> Result<Vec<IssueInfo>, GhError> {
        self.issue_list_calls
            .borrow_mut()
            .push((repo.to_string(), filter.clone()));
        self.issue_list_result.borrow().clone()
    }

    fn issue_create(&self, repo: &str, req: &IssueCreateRequest) -> Result<IssueInfo, GhError> {
        self.issue_create_calls
            .borrow_mut()
            .push((repo.to_string(), req.clone()));
        self.issue_create_result.borrow().clone()
    }

    fn issue_edit(&self, repo: &str, number: u64, req: &IssueEditRequest) -> Result<(), GhError> {
        self.issue_edit_calls
            .borrow_mut()
            .push((repo.to_string(), number, req.clone()));
        self.issue_edit_result.borrow().clone()
    }

    fn issue_comment(&self, repo: &str, number: u64, body: &str) -> Result<(), GhError> {
        self.issue_comment_calls
            .borrow_mut()
            .push((repo.to_string(), number, body.to_string()));
        self.issue_comment_result.borrow().clone()
    }

    fn issue_dependencies(&self, repo: &str, number: u64) -> Result<IssueDependencies, GhError> {
        self.issue_dependencies_calls
            .borrow_mut()
            .push((repo.to_string(), number));
        match self.issue_dependencies_results.borrow().get(&number) {
            Some(result) => result.clone(),
            None => Ok(IssueDependencies::default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- GhError::is_permanent ---
    //
    // These pin the permanent-vs-transient distinction the resilience fix
    // depends on: `resolve_blocker_stacking` (src/work/run.rs) and
    // `run_poll_loop` (src/work/review_watch.rs) both need to tell "tm asked
    // gh something nonsensical, and always will" apart from "the network/gh
    // itself hiccuped", so a permanent bug can't masquerade as a retryable
    // blip ever again.

    #[test]
    fn is_permanent_true_for_unknown_json_field() {
        let err = GhError::Command {
            command: "gh pr view".to_string(),
            exit_code: Some(1),
            stderr: r#"unknown JSON field: "merged""#.to_string(),
        };
        assert!(err.is_permanent());
    }

    #[test]
    fn is_permanent_true_for_unknown_json_field_case_insensitively() {
        let err = GhError::Command {
            command: "gh pr view".to_string(),
            exit_code: Some(1),
            stderr: "Unknown JSON field: \"merged\"".to_string(),
        };
        assert!(err.is_permanent());
    }

    #[test]
    fn is_permanent_true_for_unknown_flag() {
        let err = GhError::Command {
            command: "gh pr list".to_string(),
            exit_code: Some(1),
            stderr: "unknown flag: --nonexistent".to_string(),
        };
        assert!(err.is_permanent());
    }

    #[test]
    fn is_permanent_true_for_unknown_command() {
        let err = GhError::Command {
            command: "gh pr frobnicate".to_string(),
            exit_code: Some(1),
            stderr: "unknown command \"frobnicate\" for \"gh pr\"".to_string(),
        };
        assert!(err.is_permanent());
    }

    #[test]
    fn is_permanent_false_for_network_and_auth_failures() {
        // Every one of these is an environmental condition, not a tm bug —
        // none should ever be classified as permanent.
        for stderr in [
            "not authenticated",
            "connection reset by peer",
            "context deadline exceeded",
            "HTTP 503",
            "gh: rate limit exceeded",
        ] {
            let err = GhError::Command {
                command: "gh pr view".to_string(),
                exit_code: Some(1),
                stderr: stderr.to_string(),
            };
            assert!(
                !err.is_permanent(),
                "expected {stderr:?} to classify as transient"
            );
        }
    }

    #[test]
    fn is_permanent_defaults_to_false_when_unsure() {
        // An error whose stderr matches none of the known permanent markers
        // must default to transient — a misclassified transient error costs
        // a warn-and-fallback, while a misclassified permanent error would
        // wrongly hard-fail a run over an ordinary blip.
        let err = GhError::Command {
            command: "gh pr view".to_string(),
            exit_code: Some(1),
            stderr: "some completely novel gh failure mode".to_string(),
        };
        assert!(!err.is_permanent());
    }

    #[test]
    fn is_permanent_false_for_spawn_and_parse_errors() {
        let spawn = GhError::Spawn {
            command: "gh".to_string(),
            message: "No such file or directory".to_string(),
        };
        let parse = GhError::Parse {
            command: "gh pr view".to_string(),
            message: "invalid JSON".to_string(),
        };
        assert!(!spawn.is_permanent());
        assert!(!parse.is_permanent());
    }

    #[test]
    fn permanence_note_is_non_empty_for_permanent_errors() {
        let err = GhError::Command {
            command: "gh pr list".to_string(),
            exit_code: Some(1),
            stderr: r#"unknown JSON field: "merged""#.to_string(),
        };
        assert!(!permanence_note(&err).is_empty());
    }

    #[test]
    fn permanence_note_is_empty_for_transient_errors() {
        let err = GhError::Command {
            command: "gh pr list".to_string(),
            exit_code: Some(1),
            stderr: "not authenticated".to_string(),
        };
        assert_eq!(permanence_note(&err), "");
    }

    #[test]
    fn current_user_login_parses_trimmed_login() {
        assert_eq!(
            interpret_current_user_login_output(Some(0), "jowi-dev\n"),
            Some("jowi-dev".to_string())
        );
    }

    #[test]
    fn current_user_login_none_on_nonzero_exit() {
        assert_eq!(interpret_current_user_login_output(Some(1), ""), None);
    }

    #[test]
    fn current_user_login_none_on_empty_stdout() {
        assert_eq!(interpret_current_user_login_output(Some(0), "\n"), None);
    }

    #[test]
    fn fake_gh_cli_current_user_login_defaults_to_none() {
        let fake = FakeGhCli::new();
        assert_eq!(fake.current_user_login().unwrap(), None);
    }

    #[test]
    fn fake_gh_cli_current_user_login_is_configurable() {
        let fake = FakeGhCli::new().with_current_user_login(Ok(Some("jowi-dev".to_string())));
        assert_eq!(
            fake.current_user_login().unwrap(),
            Some("jowi-dev".to_string())
        );
    }

    #[test]
    fn pr_view_parses_successful_json() {
        let stdout = r#"{
            "number": 42,
            "url": "https://github.com/example/repo/pull/42",
            "title": "Fix the thing",
            "body": "Resolves PROJ-372",
            "headRefName": "proj-372-fix"
        }"#;
        let result = interpret_pr_view_output(Some(0), stdout, "").unwrap();
        let pr = result.expect("expected a PR");
        assert_eq!(pr.number, 42);
        assert_eq!(pr.head_ref_name, "proj-372-fix");
    }

    #[test]
    fn pr_view_no_pr_for_branch_returns_none() {
        let stderr = "no pull requests found for branch \"proj-372-fix\"\n";
        let result = interpret_pr_view_output(Some(1), "", stderr).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn pr_view_no_pr_message_is_matched_case_insensitively() {
        let stderr = "No pull requests found for branch\n";
        let result = interpret_pr_view_output(Some(1), "", stderr).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn pr_view_malformed_json_is_a_parse_error() {
        let err = interpret_pr_view_output(Some(0), "not json", "").unwrap_err();
        assert!(matches!(err, GhError::Parse { .. }));
    }

    #[test]
    fn pr_view_other_failure_is_a_command_error() {
        let err = interpret_pr_view_output(Some(1), "", "gh: authentication required").unwrap_err();
        match err {
            GhError::Command { stderr, .. } => {
                assert!(stderr.contains("authentication required"))
            }
            other => panic!("expected Command error, got {other:?}"),
        }
    }

    #[test]
    fn pr_view_signal_termination_is_a_command_error() {
        let err = interpret_pr_view_output(None, "", "").unwrap_err();
        assert!(matches!(
            err,
            GhError::Command {
                exit_code: None,
                ..
            }
        ));
    }

    #[test]
    fn current_branch_success_returns_trimmed_name() {
        let branch = interpret_current_branch_output(Some(0), "main\n", "").unwrap();
        assert_eq!(branch, "main");
    }

    #[test]
    fn current_branch_failure_is_a_command_error() {
        let err = interpret_current_branch_output(Some(1), "", "fatal: not a git repository")
            .unwrap_err();
        match err {
            GhError::Command { stderr, .. } => {
                assert!(stderr.contains("not a git repository"))
            }
            other => panic!("expected Command error, got {other:?}"),
        }
    }

    #[test]
    fn fake_gh_cli_records_pr_create_and_pr_edit_calls() {
        let fake = FakeGhCli::new();

        let create_req = PrCreateRequest {
            title: "Fix the thing".to_string(),
            body: "Resolves PROJ-372".to_string(),
            base: None,
        };
        fake.pr_create(&create_req).unwrap();

        let edit_req = PrEditRequest {
            title: Some("[PROJ-372] Fix the thing".to_string()),
            body: None,
        };
        fake.pr_edit(42, &edit_req).unwrap();

        assert_eq!(fake.pr_create_calls(), vec![create_req]);
        assert_eq!(fake.pr_edit_calls(), vec![(42, edit_req)]);
    }

    #[test]
    fn fake_gh_cli_records_pr_comment_calls() {
        let fake = FakeGhCli::new();

        fake.pr_comment(42, "Looks good to me.").unwrap();

        assert_eq!(
            fake.pr_comment_calls(),
            vec![(42, "Looks good to me.".to_string())]
        );
    }

    #[test]
    fn fake_gh_cli_pr_comment_seeded_error_is_returned() {
        let fake = FakeGhCli::new().with_pr_comment_result(Err(GhError::Command {
            command: "gh pr comment".to_string(),
            exit_code: Some(1),
            stderr: "boom".to_string(),
        }));

        let err = fake.pr_comment(42, "body").unwrap_err();
        assert!(matches!(err, GhError::Command { .. }));
    }

    #[test]
    fn fake_gh_cli_returns_configured_pr_view_result() {
        let pr = PrInfo {
            number: 1,
            url: "https://github.com/example/repo/pull/1".to_string(),
            title: "Fix the thing".to_string(),
            body: String::new(),
            head_ref_name: "proj-372-fix".to_string(),
        };
        let fake = FakeGhCli::new().with_pr_view(Ok(Some(pr.clone())));

        assert_eq!(fake.pr_view().unwrap(), Some(pr));
    }

    #[test]
    fn pr_create_success_is_ok() {
        interpret_pr_create_output(Some(0), "").unwrap();
    }

    #[test]
    fn pr_create_failure_is_a_command_error() {
        let err =
            interpret_pr_create_output(Some(1), "gh: a pull request already exists").unwrap_err();
        match err {
            GhError::Command {
                command, stderr, ..
            } => {
                assert_eq!(command, "gh pr create");
                assert!(stderr.contains("already exists"));
            }
            other => panic!("expected Command error, got {other:?}"),
        }
    }

    #[test]
    fn pr_create_signal_termination_is_a_command_error() {
        let err = interpret_pr_create_output(None, "").unwrap_err();
        assert!(matches!(
            err,
            GhError::Command {
                exit_code: None,
                ..
            }
        ));
    }

    #[test]
    fn pr_edit_success_is_ok() {
        interpret_pr_edit_output(Some(0), "").unwrap();
    }

    #[test]
    fn pr_edit_failure_is_a_command_error() {
        let err = interpret_pr_edit_output(Some(1), "gh: pull request not found").unwrap_err();
        match err {
            GhError::Command {
                command, stderr, ..
            } => {
                assert_eq!(command, "gh pr edit");
                assert!(stderr.contains("not found"));
            }
            other => panic!("expected Command error, got {other:?}"),
        }
    }

    #[test]
    fn pr_edit_signal_termination_is_a_command_error() {
        let err = interpret_pr_edit_output(None, "").unwrap_err();
        assert!(matches!(
            err,
            GhError::Command {
                exit_code: None,
                ..
            }
        ));
    }

    #[test]
    fn pr_comment_success_is_ok() {
        interpret_pr_comment_output(Some(0), "").unwrap();
    }

    #[test]
    fn pr_comment_failure_is_a_command_error() {
        let err = interpret_pr_comment_output(Some(1), "gh: pull request not found").unwrap_err();
        match err {
            GhError::Command {
                command, stderr, ..
            } => {
                assert_eq!(command, "gh pr comment");
                assert!(stderr.contains("not found"));
            }
            other => panic!("expected Command error, got {other:?}"),
        }
    }

    #[test]
    fn pr_comment_signal_termination_is_a_command_error() {
        let err = interpret_pr_comment_output(None, "").unwrap_err();
        assert!(matches!(
            err,
            GhError::Command {
                exit_code: None,
                ..
            }
        ));
    }

    #[test]
    fn pr_list_all_classifies_open_merged_and_closed() {
        let stdout = r#"[
            {"number":1,"headRefName":"jowi-dev/ax-410-open","state":"OPEN","updatedAt":"2026-08-01T00:00:00Z"},
            {"number":2,"headRefName":"jowi-dev/ax-410-merged","state":"MERGED","updatedAt":"2026-08-02T00:00:00Z"},
            {"number":3,"headRefName":"jowi-dev/ax-410-closed","state":"CLOSED","updatedAt":"2026-08-03T00:00:00Z"}
        ]"#;
        let prs = interpret_pr_list_all_output(Some(0), stdout, "").unwrap();
        assert_eq!(prs[0].lifecycle, PrLifecycle::Open);
        assert_eq!(prs[1].lifecycle, PrLifecycle::Merged);
        assert_eq!(prs[2].lifecycle, PrLifecycle::Closed);
    }

    #[test]
    fn pr_list_all_failure_is_a_command_error() {
        let err =
            interpret_pr_list_all_output(Some(1), "", "gh: authentication required").unwrap_err();
        assert!(matches!(err, GhError::Command { .. }));
    }

    #[test]
    fn fake_gh_cli_pr_list_all_records_dir_of_each_call() {
        let fake = FakeGhCli::new();
        fake.pr_list_all(Path::new("/repo-a")).unwrap();
        fake.pr_list_all(Path::new("/repo-b")).unwrap();
        assert_eq!(
            fake.pr_list_all_calls(),
            vec![PathBuf::from("/repo-a"), PathBuf::from("/repo-b")]
        );
    }

    #[test]
    fn repo_view_parses_owner_and_name() {
        let stdout = r#"{"owner":{"login":"example"},"name":"repo"}"#;
        let repo = interpret_repo_view_output(Some(0), stdout, "").unwrap();
        assert_eq!(repo.owner, "example");
        assert_eq!(repo.name, "repo");
    }

    #[test]
    fn repo_view_malformed_json_is_a_parse_error() {
        let err = interpret_repo_view_output(Some(0), "not json", "").unwrap_err();
        assert!(matches!(err, GhError::Parse { .. }));
    }

    #[test]
    fn repo_view_failure_is_a_command_error() {
        let err =
            interpret_repo_view_output(Some(1), "", "gh: authentication required").unwrap_err();
        assert!(matches!(err, GhError::Command { .. }));
    }

    #[test]
    fn review_threads_parses_resolved_and_author() {
        let stdout = r#"{
            "data": {
                "repository": {
                    "pullRequest": {
                        "reviewThreads": {
                            "nodes": [
                                {
                                    "isResolved": true,
                                    "comments": { "nodes": [{ "author": { "login": "cursor" } }] }
                                },
                                {
                                    "isResolved": false,
                                    "comments": { "nodes": [{ "author": { "login": "someone" } }] }
                                }
                            ]
                        }
                    }
                }
            }
        }"#;
        let threads = interpret_review_threads_output(Some(0), stdout, "", 42).unwrap();
        assert_eq!(
            threads,
            vec![
                ReviewThread {
                    is_resolved: true,
                    author_login: Some("cursor".to_string()),
                },
                ReviewThread {
                    is_resolved: false,
                    author_login: Some("someone".to_string()),
                },
            ]
        );
    }

    #[test]
    fn review_threads_null_author_becomes_none() {
        let stdout = r#"{
            "data": {
                "repository": {
                    "pullRequest": {
                        "reviewThreads": {
                            "nodes": [
                                {
                                    "isResolved": false,
                                    "comments": { "nodes": [{ "author": null }] }
                                }
                            ]
                        }
                    }
                }
            }
        }"#;
        let threads = interpret_review_threads_output(Some(0), stdout, "", 42).unwrap();
        assert_eq!(threads[0].author_login, None);
    }

    #[test]
    fn review_threads_null_pull_request_is_a_parse_error_naming_number() {
        let stdout = r#"{"data":{"repository":{"pullRequest":null}}}"#;
        let err = interpret_review_threads_output(Some(0), stdout, "", 42).unwrap_err();
        match err {
            GhError::Parse { message, .. } => assert!(message.contains("42")),
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn review_threads_failure_is_a_command_error() {
        let err = interpret_review_threads_output(Some(1), "", "gh: not found", 42).unwrap_err();
        assert!(matches!(err, GhError::Command { .. }));
    }

    #[test]
    fn finding_details_parses_body_path_line_url_and_author() {
        let stdout = r#"{
            "data": {
                "repository": {
                    "pullRequest": {
                        "reviewThreads": {
                            "nodes": [
                                {
                                    "isResolved": false,
                                    "comments": {
                                        "nodes": [{
                                            "author": { "login": "cursor" },
                                            "body": "This looks off.",
                                            "path": "src/lib.rs",
                                            "line": 42,
                                            "url": "https://github.com/example/repo/pull/1#comment-1"
                                        }]
                                    }
                                }
                            ]
                        }
                    }
                }
            }
        }"#;
        let details = interpret_finding_details_output(Some(0), stdout, "", 42).unwrap();
        assert_eq!(
            details,
            vec![FindingDetail {
                author_login: Some("cursor".to_string()),
                is_resolved: false,
                path: Some("src/lib.rs".to_string()),
                line: Some(42),
                body: "This looks off.".to_string(),
                url: "https://github.com/example/repo/pull/1#comment-1".to_string(),
            }]
        );
    }

    #[test]
    fn finding_details_null_author_becomes_none() {
        let stdout = r#"{
            "data": {
                "repository": {
                    "pullRequest": {
                        "reviewThreads": {
                            "nodes": [
                                {
                                    "isResolved": true,
                                    "comments": {
                                        "nodes": [{
                                            "author": null,
                                            "body": "note",
                                            "path": null,
                                            "line": null,
                                            "url": "https://github.com/example/repo/pull/1#comment-2"
                                        }]
                                    }
                                }
                            ]
                        }
                    }
                }
            }
        }"#;
        let details = interpret_finding_details_output(Some(0), stdout, "", 42).unwrap();
        assert_eq!(details[0].author_login, None);
        assert_eq!(details[0].path, None);
        assert_eq!(details[0].line, None);
        assert!(details[0].is_resolved);
    }

    #[test]
    fn finding_details_null_pull_request_is_a_parse_error_naming_number() {
        let stdout = r#"{"data":{"repository":{"pullRequest":null}}}"#;
        let err = interpret_finding_details_output(Some(0), stdout, "", 42).unwrap_err();
        match err {
            GhError::Parse { message, .. } => assert!(message.contains("42")),
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn finding_details_failure_is_a_command_error() {
        let err = interpret_finding_details_output(Some(1), "", "gh: not found", 42).unwrap_err();
        assert!(matches!(err, GhError::Command { .. }));
    }

    #[test]
    fn fake_gh_cli_returns_configured_pr_bot_finding_details_for_pr_number() {
        let details = vec![FindingDetail {
            author_login: Some("cursor".to_string()),
            is_resolved: false,
            path: Some("src/lib.rs".to_string()),
            line: Some(1),
            body: "finding".to_string(),
            url: "https://github.com/example/repo/pull/1#comment-1".to_string(),
        }];
        let fake = FakeGhCli::new().with_pr_bot_finding_details(42, Ok(details.clone()));

        assert_eq!(
            fake.pr_bot_finding_details(Path::new("/repo"), 42).unwrap(),
            details
        );
        assert_eq!(fake.pr_bot_finding_details_calls(), vec![42]);
    }

    #[test]
    fn fake_gh_cli_unconfigured_pr_bot_finding_details_returns_empty() {
        let fake = FakeGhCli::new();
        assert_eq!(
            fake.pr_bot_finding_details(Path::new("/repo"), 99).unwrap(),
            Vec::new()
        );
    }

    #[test]
    fn pr_list_parses_number_url_title_body_and_branch() {
        let stdout = r#"[
            {
                "number": 1,
                "url": "https://github.com/example/repo/pull/1",
                "title": "Fix the thing",
                "body": "Resolves PROJ-372",
                "headRefName": "proj-372-fix"
            },
            {
                "number": 2,
                "url": "https://github.com/example/repo/pull/2",
                "title": "Add the widget",
                "body": "",
                "headRefName": "add-widget"
            }
        ]"#;
        let prs = interpret_pr_list_output(Some(0), stdout, "").unwrap();
        assert_eq!(
            prs,
            vec![
                PrInfo {
                    number: 1,
                    url: "https://github.com/example/repo/pull/1".to_string(),
                    title: "Fix the thing".to_string(),
                    body: "Resolves PROJ-372".to_string(),
                    head_ref_name: "proj-372-fix".to_string(),
                },
                PrInfo {
                    number: 2,
                    url: "https://github.com/example/repo/pull/2".to_string(),
                    title: "Add the widget".to_string(),
                    body: String::new(),
                    head_ref_name: "add-widget".to_string(),
                },
            ]
        );
    }

    #[test]
    fn pr_url_for_branch_parses_first_url() {
        let stdout = r#"[{"url":"https://github.com/example/repo/pull/7"}]"#;
        assert_eq!(
            interpret_pr_url_for_branch_output(Some(0), stdout),
            Some("https://github.com/example/repo/pull/7".to_string())
        );
    }

    #[test]
    fn pr_url_for_branch_empty_array_is_none() {
        assert_eq!(interpret_pr_url_for_branch_output(Some(0), "[]"), None);
    }

    #[test]
    fn pr_url_for_branch_nonzero_exit_is_none() {
        assert_eq!(interpret_pr_url_for_branch_output(Some(1), ""), None);
    }

    #[test]
    fn pr_url_for_branch_malformed_json_is_none() {
        assert_eq!(
            interpret_pr_url_for_branch_output(Some(0), "not json"),
            None
        );
    }

    #[test]
    fn fake_gh_cli_pr_url_for_branch_records_call_and_returns_configured_result() {
        let fake = FakeGhCli::new().with_pr_url_for_branch(Ok(Some(
            "https://github.com/example/repo/pull/9".to_string(),
        )));

        assert_eq!(
            fake.pr_url_for_branch("claude/mylane-20260101-090503")
                .unwrap(),
            Some("https://github.com/example/repo/pull/9".to_string())
        );
        assert_eq!(
            fake.pr_url_for_branch_calls(),
            vec!["claude/mylane-20260101-090503".to_string()]
        );
    }

    #[test]
    fn fake_gh_cli_pr_url_for_branch_defaults_to_none() {
        let fake = FakeGhCli::new();
        assert_eq!(fake.pr_url_for_branch("some-branch").unwrap(), None);
    }

    #[test]
    fn pr_list_failure_is_a_command_error() {
        let err = interpret_pr_list_output(Some(1), "", "gh: authentication required").unwrap_err();
        assert!(matches!(err, GhError::Command { .. }));
    }

    #[test]
    fn fake_gh_cli_returns_configured_review_threads_for_pr_number() {
        let threads = vec![ReviewThread {
            is_resolved: false,
            author_login: Some("cursor".to_string()),
        }];
        let fake = FakeGhCli::new().with_review_threads(42, Ok(threads.clone()));

        assert_eq!(
            fake.pr_review_threads(Path::new("/repo"), 42).unwrap(),
            threads
        );
        assert_eq!(fake.pr_review_threads_calls(), vec![42]);
    }

    #[test]
    fn fake_gh_cli_unconfigured_review_threads_returns_empty() {
        let fake = FakeGhCli::new();
        assert_eq!(
            fake.pr_review_threads(Path::new("/repo"), 99).unwrap(),
            Vec::new()
        );
    }

    #[test]
    fn pr_reviews_parses_login_and_null_author() {
        let stdout = r#"[
            {"user": {"login": "cursor[bot]"}},
            {"user": null}
        ]"#;
        let reviews = interpret_pr_reviews_output(Some(0), stdout, "").unwrap();
        assert_eq!(
            reviews,
            vec![
                PrReview {
                    author_login: Some("cursor[bot]".to_string()),
                },
                PrReview { author_login: None },
            ]
        );
    }

    #[test]
    fn pr_reviews_empty_array_is_empty_vec() {
        assert_eq!(interpret_pr_reviews_output(Some(0), "[]", "").unwrap(), []);
    }

    #[test]
    fn pr_reviews_malformed_json_is_a_parse_error() {
        let err = interpret_pr_reviews_output(Some(0), "not json", "").unwrap_err();
        assert!(matches!(err, GhError::Parse { .. }));
    }

    #[test]
    fn pr_reviews_failure_is_a_command_error() {
        let err =
            interpret_pr_reviews_output(Some(1), "", "gh: authentication required").unwrap_err();
        assert!(matches!(err, GhError::Command { .. }));
    }

    #[test]
    fn pr_state_open_when_state_open() {
        let stdout = r#"{"state":"OPEN"}"#;
        assert_eq!(
            interpret_pr_state_output(Some(0), stdout, "").unwrap(),
            PrLifecycle::Open
        );
    }

    #[test]
    fn pr_state_merged_when_state_merged() {
        let stdout = r#"{"state":"MERGED"}"#;
        assert_eq!(
            interpret_pr_state_output(Some(0), stdout, "").unwrap(),
            PrLifecycle::Merged
        );
    }

    #[test]
    fn pr_state_closed_when_state_closed() {
        let stdout = r#"{"state":"CLOSED"}"#;
        assert_eq!(
            interpret_pr_state_output(Some(0), stdout, "").unwrap(),
            PrLifecycle::Closed
        );
    }

    /// Regression test for a real `gh` failure: `gh pr view` has no `merged`
    /// JSON field (only `gh pr list --json merged` used to exist, and even
    /// that field was removed from newer `gh` — see `PrLifecycle`'s doc
    /// comment). Requesting it made `gh pr view <n> --json state,merged` fail
    /// every call with `Unknown JSON field: "merged"`, which broke every `tm
    /// pr watch` poll tick. Pin the exact field list here so a future edit
    /// can't reintroduce it.
    #[test]
    fn pr_state_requests_only_the_state_field() {
        assert_eq!(PR_STATE_JSON_FIELDS, "state");
    }

    /// Same regression as [`pr_state_requests_only_the_state_field`]: `gh pr
    /// list` also has no `merged` JSON field on this `gh` version, so
    /// [`GhCli::pr_list_all`] must not request one either.
    #[test]
    fn pr_list_all_does_not_request_the_merged_field() {
        assert_eq!(
            PR_LIST_ALL_JSON_FIELDS,
            "number,headRefName,state,updatedAt"
        );
    }

    #[test]
    fn pr_state_malformed_json_is_a_parse_error() {
        let err = interpret_pr_state_output(Some(0), "not json", "").unwrap_err();
        assert!(matches!(err, GhError::Parse { .. }));
    }

    #[test]
    fn pr_state_failure_is_a_command_error() {
        let err = interpret_pr_state_output(Some(1), "", "gh: not found").unwrap_err();
        assert!(matches!(err, GhError::Command { .. }));
    }

    #[test]
    fn fake_gh_cli_returns_configured_pr_reviews_for_pr_number() {
        let reviews = vec![PrReview {
            author_login: Some("cursor[bot]".to_string()),
        }];
        let fake = FakeGhCli::new().with_pr_reviews(42, Ok(reviews.clone()));

        assert_eq!(fake.pr_reviews(Path::new("/repo"), 42).unwrap(), reviews);
        assert_eq!(fake.pr_reviews_calls(), vec![42]);
    }

    #[test]
    fn fake_gh_cli_unconfigured_pr_reviews_returns_empty() {
        let fake = FakeGhCli::new();
        assert_eq!(fake.pr_reviews(Path::new("/repo"), 99).unwrap(), Vec::new());
    }

    #[test]
    fn fake_gh_cli_returns_configured_pr_state_for_pr_number() {
        let fake = FakeGhCli::new().with_pr_state(42, Ok(PrLifecycle::Merged));

        assert_eq!(
            fake.pr_state(Path::new("/repo"), 42).unwrap(),
            PrLifecycle::Merged
        );
        assert_eq!(fake.pr_state_calls(), vec![42]);
    }

    #[test]
    fn fake_gh_cli_unconfigured_pr_state_defaults_to_open() {
        let fake = FakeGhCli::new();
        assert_eq!(
            fake.pr_state(Path::new("/repo"), 99).unwrap(),
            PrLifecycle::Open
        );
    }

    #[test]
    fn fake_gh_cli_returns_configured_pr_list() {
        let prs = vec![PrInfo {
            number: 1,
            url: "https://github.com/example/repo/pull/1".to_string(),
            title: "Fix the thing".to_string(),
            body: String::new(),
            head_ref_name: "proj-372-fix".to_string(),
        }];
        let fake = FakeGhCli::new().with_pr_list(Ok(prs.clone()));
        assert_eq!(fake.pr_list(Path::new("/repo")).unwrap(), prs);
    }

    #[test]
    fn fake_gh_cli_pr_list_records_dir_of_each_call() {
        let fake = FakeGhCli::new();
        fake.pr_list(Path::new("/repo-a")).unwrap();
        fake.pr_list(Path::new("/repo-b")).unwrap();
        assert_eq!(
            fake.pr_list_calls(),
            vec![PathBuf::from("/repo-a"), PathBuf::from("/repo-b")]
        );
    }

    #[test]
    fn fake_gh_cli_pr_list_bounded_unconfigured_delegates_to_pr_list() {
        let prs = vec![PrInfo {
            number: 1,
            url: "https://github.com/example/repo/pull/1".to_string(),
            title: "Fix the thing".to_string(),
            body: String::new(),
            head_ref_name: "proj-372-fix".to_string(),
        }];
        let fake = FakeGhCli::new().with_pr_list(Ok(prs.clone()));
        assert_eq!(
            fake.pr_list_bounded(Path::new("/repo"), Duration::from_secs(8))
                .unwrap(),
            prs
        );
    }

    #[test]
    fn fake_gh_cli_pr_list_bounded_configured_overrides_pr_list() {
        let fake = FakeGhCli::new()
            .with_pr_list(Ok(vec![]))
            .with_pr_list_bounded(Err(GhError::Timeout {
                command: "gh pr list".to_string(),
                seconds: 8,
            }));
        let err = fake
            .pr_list_bounded(Path::new("/repo"), Duration::from_secs(8))
            .unwrap_err();
        assert!(matches!(err, GhError::Timeout { .. }));
        assert_eq!(fake.pr_list_bounded_calls(), vec![PathBuf::from("/repo")]);
    }

    #[test]
    fn is_permanent_false_for_timeout() {
        let err = GhError::Timeout {
            command: "gh pr list".to_string(),
            seconds: 8,
        };
        assert!(!err.is_permanent());
    }

    // --- spawn_with_timeout ---
    //
    // Tested directly against ordinary subprocesses (not `gh`) so the
    // kill-on-timeout mechanism itself is covered without needing a real `gh`
    // binary or network -- see the function's doc comment.

    #[test]
    fn spawn_with_timeout_returns_output_for_a_fast_command() {
        let mut command = Command::new("echo");
        command.arg("hello");
        let output = spawn_with_timeout(command, "echo", Duration::from_secs(5)).unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hello");
    }

    #[test]
    fn spawn_with_timeout_kills_and_errors_on_a_hanging_command() {
        let mut command = Command::new("sleep");
        command.arg("30");
        let start = std::time::Instant::now();
        let err = spawn_with_timeout(command, "sleep", Duration::from_millis(200)).unwrap_err();
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "should time out quickly, not wait for the full sleep"
        );
        match err {
            GhError::Timeout { command, seconds } => {
                assert_eq!(command, "sleep");
                assert_eq!(seconds, 0);
            }
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    // --- issue_view ---

    #[test]
    fn issue_view_parses_successful_json() {
        let stdout = r#"{
            "number": 3,
            "url": "https://github.com/jowi-dev/tskmstr/issues/3",
            "title": "GitHub Issues as a ticket backend",
            "body": "Goal section",
            "state": "OPEN",
            "labels": [{"name": "tm:status/in-progress"}, {"name": "enhancement"}],
            "assignees": [{"login": "jowi-dev"}]
        }"#;
        let issue = interpret_issue_view_output("gh issue view", Some(0), stdout, "").unwrap();
        assert_eq!(issue.number, 3);
        assert_eq!(issue.state, IssueState::Open);
        assert_eq!(
            issue.labels,
            vec![
                "tm:status/in-progress".to_string(),
                "enhancement".to_string()
            ]
        );
        assert_eq!(issue.assignees, vec!["jowi-dev".to_string()]);
    }

    #[test]
    fn issue_view_closed_state_parses() {
        let stdout = r#"{
            "number": 1, "url": "u", "title": "t", "body": "b",
            "state": "CLOSED", "labels": [], "assignees": []
        }"#;
        let issue = interpret_issue_view_output("gh issue view", Some(0), stdout, "").unwrap();
        assert_eq!(issue.state, IssueState::Closed);
    }

    #[test]
    fn issue_view_malformed_json_is_a_parse_error() {
        let err =
            interpret_issue_view_output("gh issue view", Some(0), "not json", "").unwrap_err();
        assert!(matches!(err, GhError::Parse { .. }));
    }

    #[test]
    fn issue_view_failure_is_a_command_error() {
        let err = interpret_issue_view_output("gh issue view", Some(1), "", "gh: issue not found")
            .unwrap_err();
        match err {
            GhError::Command { stderr, .. } => assert!(stderr.contains("not found")),
            other => panic!("expected Command error, got {other:?}"),
        }
    }

    #[test]
    fn issue_view_signal_termination_is_a_command_error() {
        let err = interpret_issue_view_output("gh issue view", None, "", "").unwrap_err();
        assert!(matches!(
            err,
            GhError::Command {
                exit_code: None,
                ..
            }
        ));
    }

    #[test]
    fn fake_gh_cli_issue_view_records_calls_and_returns_configured_result() {
        let issue = IssueInfo {
            number: 3,
            url: "https://github.com/jowi-dev/tskmstr/issues/3".to_string(),
            title: "GitHub Issues as a ticket backend".to_string(),
            body: String::new(),
            state: IssueState::Open,
            labels: vec!["tm:status/todo".to_string()],
            assignees: Vec::new(),
        };
        let fake = FakeGhCli::new().with_issue_view(3, Ok(issue.clone()));

        assert_eq!(fake.issue_view("jowi-dev/tskmstr", 3).unwrap(), issue);
        assert_eq!(
            fake.issue_view_calls(),
            vec![("jowi-dev/tskmstr".to_string(), 3)]
        );
    }

    #[test]
    fn fake_gh_cli_issue_view_unconfigured_number_is_an_error() {
        let fake = FakeGhCli::new();
        let err = fake.issue_view("jowi-dev/tskmstr", 99).unwrap_err();
        assert!(matches!(err, GhError::Command { .. }));
    }

    // --- issue_list ---

    #[test]
    fn issue_list_args_include_state_limit_and_json_fields() {
        let filter = IssueListFilter::default();
        let args = issue_list_args("jowi-dev/tskmstr", &filter);
        assert_eq!(
            args,
            vec![
                "issue",
                "list",
                "-R",
                "jowi-dev/tskmstr",
                "--state",
                "open",
                "--limit",
                "200",
                "--json",
                ISSUE_JSON_FIELDS,
            ]
        );
    }

    #[test]
    fn issue_list_args_include_labels_and_assignee_when_set() {
        let filter = IssueListFilter {
            state: IssueListState::All,
            labels: vec!["tm:status/todo".to_string(), "bug".to_string()],
            assignee: Some("jowi-dev".to_string()),
            limit: 50,
        };
        let args = issue_list_args("jowi-dev/tskmstr", &filter);
        assert!(args.contains(&"--label".to_string()));
        assert!(args.contains(&"tm:status/todo,bug".to_string()));
        assert!(args.contains(&"--assignee".to_string()));
        assert!(args.contains(&"jowi-dev".to_string()));
        assert!(args.contains(&"all".to_string()));
    }

    #[test]
    fn issue_list_parses_successful_json_array() {
        let stdout = r#"[
            {"number": 1, "url": "u1", "title": "t1", "body": "", "state": "OPEN", "labels": [], "assignees": []},
            {"number": 2, "url": "u2", "title": "t2", "body": "", "state": "CLOSED", "labels": [], "assignees": []}
        ]"#;
        let issues = interpret_issue_list_output(Some(0), stdout, "").unwrap();
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].number, 1);
        assert_eq!(issues[1].state, IssueState::Closed);
    }

    #[test]
    fn issue_list_failure_is_a_command_error() {
        let err = interpret_issue_list_output(Some(1), "", "gh: not authenticated").unwrap_err();
        assert!(matches!(err, GhError::Command { .. }));
    }

    #[test]
    fn fake_gh_cli_issue_list_records_calls_and_returns_configured_result() {
        let fake = FakeGhCli::new().with_issue_list(Ok(vec![IssueInfo {
            number: 1,
            url: String::new(),
            title: "t".to_string(),
            body: String::new(),
            state: IssueState::Open,
            labels: Vec::new(),
            assignees: Vec::new(),
        }]));
        let filter = IssueListFilter::default();

        let result = fake.issue_list("jowi-dev/tskmstr", &filter).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(
            fake.issue_list_calls(),
            vec![("jowi-dev/tskmstr".to_string(), filter)]
        );
    }

    // --- issue_create ---

    #[test]
    fn issue_create_args_omit_label_and_assignee_flags_when_empty() {
        let req = IssueCreateRequest {
            title: "Fix the thing".to_string(),
            body: "Details".to_string(),
            labels: Vec::new(),
            assignees: Vec::new(),
        };
        let args = issue_create_args("jowi-dev/tskmstr", &req);
        assert_eq!(
            args,
            vec![
                "issue",
                "create",
                "-R",
                "jowi-dev/tskmstr",
                "--title",
                "Fix the thing",
                "--body",
                "Details",
            ]
        );
    }

    #[test]
    fn issue_create_args_include_labels_and_assignees_when_set() {
        let req = IssueCreateRequest {
            title: "Fix the thing".to_string(),
            body: "Details".to_string(),
            labels: vec!["tm:status/todo".to_string()],
            assignees: vec!["jowi-dev".to_string()],
        };
        let args = issue_create_args("jowi-dev/tskmstr", &req);
        assert!(args.contains(&"--label".to_string()));
        assert!(args.contains(&"tm:status/todo".to_string()));
        assert!(args.contains(&"--assignee".to_string()));
        assert!(args.contains(&"jowi-dev".to_string()));
    }

    #[test]
    fn parse_issue_number_from_url_extracts_trailing_segment() {
        assert_eq!(
            parse_issue_number_from_url("https://github.com/jowi-dev/tskmstr/issues/42\n"),
            Some(42)
        );
    }

    #[test]
    fn parse_issue_number_from_url_none_for_non_numeric_segment() {
        assert_eq!(
            parse_issue_number_from_url("https://github.com/jowi-dev/tskmstr"),
            None
        );
    }

    #[test]
    fn issue_create_success_returns_parsed_number() {
        let number = interpret_issue_create_output(
            Some(0),
            "https://github.com/jowi-dev/tskmstr/issues/42\n",
            "",
        )
        .unwrap();
        assert_eq!(number, 42);
    }

    #[test]
    fn issue_create_failure_is_a_command_error() {
        let err = interpret_issue_create_output(Some(1), "", "gh: validation failed").unwrap_err();
        match err {
            GhError::Command { stderr, .. } => assert!(stderr.contains("validation failed")),
            other => panic!("expected Command error, got {other:?}"),
        }
    }

    #[test]
    fn issue_create_unparseable_url_is_a_parse_error() {
        let err = interpret_issue_create_output(Some(0), "not a url", "").unwrap_err();
        assert!(matches!(err, GhError::Parse { .. }));
    }

    #[test]
    fn fake_gh_cli_issue_create_records_calls_and_returns_configured_result() {
        let created = IssueInfo {
            number: 42,
            url: "https://github.com/jowi-dev/tskmstr/issues/42".to_string(),
            title: "Fix the thing".to_string(),
            body: String::new(),
            state: IssueState::Open,
            labels: Vec::new(),
            assignees: Vec::new(),
        };
        let fake = FakeGhCli::new().with_issue_create_result(Ok(created.clone()));
        let req = IssueCreateRequest {
            title: "Fix the thing".to_string(),
            body: "Details".to_string(),
            labels: Vec::new(),
            assignees: Vec::new(),
        };

        let result = fake.issue_create("jowi-dev/tskmstr", &req).unwrap();
        assert_eq!(result, created);
        assert_eq!(
            fake.issue_create_calls(),
            vec![("jowi-dev/tskmstr".to_string(), req)]
        );
    }

    // --- issue_edit ---

    #[test]
    fn issue_edit_args_none_when_no_label_or_assignee_change() {
        let req = IssueEditRequest {
            state: Some(IssueStateChange::Close),
            ..Default::default()
        };
        assert_eq!(issue_edit_args("jowi-dev/tskmstr", 3, &req), None);
    }

    #[test]
    fn issue_edit_args_build_add_and_remove_flags() {
        let req = IssueEditRequest {
            add_labels: vec!["tm:status/in-progress".to_string()],
            remove_labels: vec!["tm:status/todo".to_string()],
            add_assignees: vec!["jowi-dev".to_string()],
            remove_assignees: vec!["other-dev".to_string()],
            state: None,
        };
        let args = issue_edit_args("jowi-dev/tskmstr", 3, &req).unwrap();
        assert_eq!(
            args,
            vec![
                "issue",
                "edit",
                "3",
                "-R",
                "jowi-dev/tskmstr",
                "--add-label",
                "tm:status/in-progress",
                "--remove-label",
                "tm:status/todo",
                "--add-assignee",
                "jowi-dev",
                "--remove-assignee",
                "other-dev",
            ]
        );
    }

    #[test]
    fn fake_gh_cli_issue_edit_records_calls() {
        let fake = FakeGhCli::new();
        let req = IssueEditRequest {
            add_labels: vec!["tm:status/in-progress".to_string()],
            state: Some(IssueStateChange::Close),
            ..Default::default()
        };

        fake.issue_edit("jowi-dev/tskmstr", 3, &req).unwrap();

        assert_eq!(
            fake.issue_edit_calls(),
            vec![("jowi-dev/tskmstr".to_string(), 3, req)]
        );
    }

    #[test]
    fn fake_gh_cli_issue_edit_seeded_error_is_returned() {
        let fake = FakeGhCli::new().with_issue_edit_result(Err(GhError::Command {
            command: "gh issue edit".to_string(),
            exit_code: Some(1),
            stderr: "boom".to_string(),
        }));

        let err = fake
            .issue_edit("jowi-dev/tskmstr", 3, &IssueEditRequest::default())
            .unwrap_err();
        assert!(matches!(err, GhError::Command { .. }));
    }

    // --- issue_comment ---

    #[test]
    fn fake_gh_cli_records_issue_comment_calls() {
        let fake = FakeGhCli::new();

        fake.issue_comment("jowi-dev/tskmstr", 3, "Looks good.")
            .unwrap();

        assert_eq!(
            fake.issue_comment_calls(),
            vec![("jowi-dev/tskmstr".to_string(), 3, "Looks good.".to_string())]
        );
    }

    #[test]
    fn fake_gh_cli_issue_comment_seeded_error_is_returned() {
        let fake = FakeGhCli::new().with_issue_comment_result(Err(GhError::Command {
            command: "gh issue comment".to_string(),
            exit_code: Some(1),
            stderr: "boom".to_string(),
        }));

        let err = fake
            .issue_comment("jowi-dev/tskmstr", 3, "body")
            .unwrap_err();
        assert!(matches!(err, GhError::Command { .. }));
    }

    // --- issue_dependencies ---

    #[test]
    fn split_repo_slug_splits_owner_and_name() {
        assert_eq!(
            split_repo_slug("jowi-dev/tskmstr").unwrap(),
            ("jowi-dev", "tskmstr")
        );
    }

    #[test]
    fn split_repo_slug_rejects_malformed_slugs() {
        assert!(split_repo_slug("no-slash").is_err());
        assert!(split_repo_slug("/tskmstr").is_err());
        assert!(split_repo_slug("jowi-dev/").is_err());
        assert!(split_repo_slug("jowi-dev/tskmstr/extra").is_err());
    }

    #[test]
    fn issue_dependencies_parses_blocked_by_and_blocking() {
        let stdout = r#"{
            "data": {
                "repository": {
                    "issue": {
                        "blockedBy": {
                            "nodes": [
                                {"number": 1, "title": "Provider trait", "state": "CLOSED", "url": "u1"}
                            ]
                        },
                        "blocking": {
                            "nodes": [
                                {"number": 5, "title": "Dogfood", "state": "OPEN", "url": "u5"}
                            ]
                        }
                    }
                }
            }
        }"#;
        let deps = interpret_issue_dependencies_output(Some(0), stdout, "", 3).unwrap();
        assert_eq!(deps.blocked_by.len(), 1);
        assert_eq!(deps.blocked_by[0].number, 1);
        assert_eq!(deps.blocked_by[0].state, IssueState::Closed);
        assert_eq!(deps.blocking.len(), 1);
        assert_eq!(deps.blocking[0].number, 5);
    }

    #[test]
    fn issue_dependencies_null_issue_is_a_parse_error_naming_the_number() {
        let stdout = r#"{"data": {"repository": {"issue": null}}}"#;
        let err = interpret_issue_dependencies_output(Some(0), stdout, "", 3).unwrap_err();
        match err {
            GhError::Parse { message, .. } => assert!(message.contains('3')),
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn issue_dependencies_failure_is_a_command_error() {
        let err = interpret_issue_dependencies_output(Some(1), "", "gh: rate limit exceeded", 3)
            .unwrap_err();
        assert!(matches!(err, GhError::Command { .. }));
    }

    #[test]
    fn fake_gh_cli_issue_dependencies_defaults_to_empty() {
        let fake = FakeGhCli::new();
        let deps = fake.issue_dependencies("jowi-dev/tskmstr", 3).unwrap();
        assert_eq!(deps, IssueDependencies::default());
        assert_eq!(
            fake.issue_dependencies_calls(),
            vec![("jowi-dev/tskmstr".to_string(), 3)]
        );
    }

    #[test]
    fn fake_gh_cli_issue_dependencies_returns_configured_result() {
        let deps = IssueDependencies {
            blocked_by: vec![IssueRef {
                number: 1,
                title: "Provider trait".to_string(),
                state: IssueState::Closed,
                url: "u1".to_string(),
            }],
            blocking: Vec::new(),
        };
        let fake = FakeGhCli::new().with_issue_dependencies(3, Ok(deps.clone()));

        assert_eq!(
            fake.issue_dependencies("jowi-dev/tskmstr", 3).unwrap(),
            deps
        );
    }
}
