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
}

/// The lifecycle state of a pull request, as reported by `gh pr view --json
/// state,merged`.
///
/// `gh`'s `state` field alone only distinguishes `OPEN`/`CLOSED` (merged PRs
/// still report `CLOSED`), so this also consults the separate `merged`
/// boolean to tell "closed because merged" apart from "closed unmerged".
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
    fn pr_review_threads(&self, number: u64) -> Result<Vec<ReviewThread>, GhError>;

    /// List the review submissions on pull request `number`, one entry per
    /// review regardless of how many comments (if any) it left.
    ///
    /// Unlike [`GhCli::pr_review_threads`] (GraphQL-only, unsuffixed bot
    /// logins), this shells out to `gh api
    /// repos/{owner}/{repo}/pulls/{number}/reviews` (REST), which reports bot
    /// logins *with* the `[bot]` suffix. This is the only way to see that a
    /// bot has run at all: a bot that finds nothing posts a review with zero
    /// comments, leaving no trace in review-thread data.
    fn pr_reviews(&self, number: u64) -> Result<Vec<PrReview>, GhError>;

    /// The lifecycle state of pull request `number` (`gh pr view <number>
    /// --json state,merged`).
    fn pr_state(&self, number: u64) -> Result<PrLifecycle, GhError>;

    /// List the review threads on pull request `number`, same as
    /// [`GhCli::pr_review_threads`] but with each thread's first comment's
    /// full body/path/line/URL, for handing bot findings to a cleanup
    /// session prompt. A separate GraphQL query from
    /// [`GhCli::pr_review_threads`]'s: that counting path never needed
    /// comment bodies/locations, so this doesn't touch it.
    fn pr_bot_finding_details(&self, number: u64) -> Result<Vec<FindingDetail>, GhError>;

    /// List open pull requests in the current repository, fetching the same
    /// fields as [`GhCli::pr_view`] (`gh pr list --state open --json
    /// number,url,title,body,headRefName`) so one [`PrInfo`] parser serves
    /// both and callers like [`super::pr::find_pr_for_ticket`] have the
    /// title/body/branch data they need to resolve a ticket key without a
    /// second lookup.
    fn pr_list(&self) -> Result<Vec<PrInfo>, GhError>;

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
    /// (`gh pr list --state all --json number,headRefName,state,merged,
    /// updatedAt`).
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

/// Fields requested from `gh pr view --json`; shared so the flag and the
/// [`PrInfo`] deserialization stay in lockstep.
const PR_VIEW_JSON_FIELDS: &str = "number,url,title,body,headRefName";

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
    /// Resolve the current repository's owner and name (`gh repo view --json
    /// owner,name`), shared by [`GhCli::pr_review_threads`] and
    /// [`GhCli::pr_reviews`], both of which need it to build a REST/GraphQL
    /// path for the repository.
    fn resolve_repo(&self) -> Result<RepoRef, GhError> {
        let output = Command::new("gh")
            .args(["repo", "view", "--json", "owner,name"])
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

    fn pr_review_threads(&self, number: u64) -> Result<Vec<ReviewThread>, GhError> {
        let repo = self.resolve_repo()?;

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

    fn pr_reviews(&self, number: u64) -> Result<Vec<PrReview>, GhError> {
        let repo = self.resolve_repo()?;

        let path = format!("repos/{}/{}/pulls/{number}/reviews", repo.owner, repo.name);
        let output = Command::new("gh")
            .args(["api", &path])
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

    fn pr_state(&self, number: u64) -> Result<PrLifecycle, GhError> {
        let output = Command::new("gh")
            .args(["pr", "view", &number.to_string(), "--json", "state,merged"])
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

    fn pr_bot_finding_details(&self, number: u64) -> Result<Vec<FindingDetail>, GhError> {
        let repo = self.resolve_repo()?;

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

    fn pr_list(&self) -> Result<Vec<PrInfo>, GhError> {
        let output = Command::new("gh")
            .args([
                "pr",
                "list",
                "--state",
                "open",
                "--json",
                PR_VIEW_JSON_FIELDS,
            ])
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
                "number,headRefName,state,merged,updatedAt",
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

/// Raw shape of `gh pr view --json state,merged` output, for deserialization
/// only.
#[derive(Debug, Deserialize)]
struct RawPrState {
    state: String,
    merged: bool,
}

/// Interpret the result of a `gh pr view <number> --json state,merged`
/// invocation.
///
/// Pure over the exit code and captured stdout/stderr, for the same
/// testability reasons as [`interpret_pr_view_output`]. `merged` takes
/// precedence over `state` (see [`PrLifecycle`]'s doc comment): a merged PR
/// reports `state: "CLOSED"` too, so `merged` is checked first.
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
            Ok(if raw.merged {
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
/// number,headRefName,state,merged,updatedAt` output, for deserialization
/// only; [`PrSummary`] is the flattened shape callers use.
#[derive(Debug, Deserialize)]
struct RawPrListAllEntry {
    number: u64,
    #[serde(rename = "headRefName")]
    head_ref_name: String,
    state: String,
    merged: bool,
    #[serde(rename = "updatedAt")]
    updated_at: String,
}

/// Interpret the result of a `gh pr list --state all --json
/// number,headRefName,state,merged,updatedAt` invocation.
///
/// Pure over the exit code and captured stdout/stderr, for the same
/// testability reasons as [`interpret_pr_view_output`]. `merged` takes
/// precedence over `state` when classifying [`PrLifecycle`], same as
/// [`interpret_pr_state_output`] (a merged PR still reports `state:
/// "CLOSED"`).
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
                        lifecycle: if entry.merged {
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
    pr_create_calls: RefCell<Vec<PrCreateRequest>>,
    pr_edit_calls: RefCell<Vec<(u64, PrEditRequest)>>,
    review_threads_results: RefCell<HashMap<u64, Result<Vec<ReviewThread>, GhError>>>,
    review_threads_calls: RefCell<Vec<u64>>,
    pr_reviews_results: RefCell<HashMap<u64, Result<Vec<PrReview>, GhError>>>,
    pr_reviews_calls: RefCell<Vec<u64>>,
    pr_state_results: RefCell<HashMap<u64, Result<PrLifecycle, GhError>>>,
    pr_state_calls: RefCell<Vec<u64>>,
    finding_details_results: RefCell<HashMap<u64, Result<Vec<FindingDetail>, GhError>>>,
    finding_details_calls: RefCell<Vec<u64>>,
    pr_list_result: RefCell<Result<Vec<PrInfo>, GhError>>,
    current_user_login_result: RefCell<Result<Option<String>, GhError>>,
    pr_url_for_branch_result: RefCell<Result<Option<String>, GhError>>,
    pr_url_for_branch_calls: RefCell<Vec<String>>,
    pr_list_all_result: RefCell<Result<Vec<PrSummary>, GhError>>,
    pr_list_all_calls: RefCell<Vec<PathBuf>>,
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
            pr_create_calls: RefCell::new(Vec::new()),
            pr_edit_calls: RefCell::new(Vec::new()),
            review_threads_results: RefCell::new(HashMap::new()),
            review_threads_calls: RefCell::new(Vec::new()),
            pr_reviews_results: RefCell::new(HashMap::new()),
            pr_reviews_calls: RefCell::new(Vec::new()),
            pr_state_results: RefCell::new(HashMap::new()),
            pr_state_calls: RefCell::new(Vec::new()),
            finding_details_results: RefCell::new(HashMap::new()),
            finding_details_calls: RefCell::new(Vec::new()),
            pr_list_result: RefCell::new(Ok(Vec::new())),
            current_user_login_result: RefCell::new(Ok(None)),
            pr_url_for_branch_result: RefCell::new(Ok(None)),
            pr_url_for_branch_calls: RefCell::new(Vec::new()),
            pr_list_all_result: RefCell::new(Ok(Vec::new())),
            pr_list_all_calls: RefCell::new(Vec::new()),
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

    fn current_branch(&self) -> Result<String, GhError> {
        self.current_branch_result.borrow().clone()
    }

    fn pr_review_threads(&self, number: u64) -> Result<Vec<ReviewThread>, GhError> {
        self.review_threads_calls.borrow_mut().push(number);
        match self.review_threads_results.borrow().get(&number) {
            Some(result) => result.clone(),
            None => Ok(Vec::new()),
        }
    }

    fn pr_reviews(&self, number: u64) -> Result<Vec<PrReview>, GhError> {
        self.pr_reviews_calls.borrow_mut().push(number);
        match self.pr_reviews_results.borrow().get(&number) {
            Some(result) => result.clone(),
            None => Ok(Vec::new()),
        }
    }

    fn pr_state(&self, number: u64) -> Result<PrLifecycle, GhError> {
        self.pr_state_calls.borrow_mut().push(number);
        match self.pr_state_results.borrow().get(&number) {
            Some(result) => result.clone(),
            None => Ok(PrLifecycle::Open),
        }
    }

    fn pr_bot_finding_details(&self, number: u64) -> Result<Vec<FindingDetail>, GhError> {
        self.finding_details_calls.borrow_mut().push(number);
        match self.finding_details_results.borrow().get(&number) {
            Some(result) => result.clone(),
            None => Ok(Vec::new()),
        }
    }

    fn pr_list(&self) -> Result<Vec<PrInfo>, GhError> {
        self.pr_list_result.borrow().clone()
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn pr_list_all_classifies_open_merged_and_closed() {
        let stdout = r#"[
            {"number":1,"headRefName":"jowi-dev/ax-410-open","state":"OPEN","merged":false,"updatedAt":"2026-08-01T00:00:00Z"},
            {"number":2,"headRefName":"jowi-dev/ax-410-merged","state":"CLOSED","merged":true,"updatedAt":"2026-08-02T00:00:00Z"},
            {"number":3,"headRefName":"jowi-dev/ax-410-closed","state":"CLOSED","merged":false,"updatedAt":"2026-08-03T00:00:00Z"}
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

        assert_eq!(fake.pr_bot_finding_details(42).unwrap(), details);
        assert_eq!(fake.pr_bot_finding_details_calls(), vec![42]);
    }

    #[test]
    fn fake_gh_cli_unconfigured_pr_bot_finding_details_returns_empty() {
        let fake = FakeGhCli::new();
        assert_eq!(fake.pr_bot_finding_details(99).unwrap(), Vec::new());
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

        assert_eq!(fake.pr_review_threads(42).unwrap(), threads);
        assert_eq!(fake.pr_review_threads_calls(), vec![42]);
    }

    #[test]
    fn fake_gh_cli_unconfigured_review_threads_returns_empty() {
        let fake = FakeGhCli::new();
        assert_eq!(fake.pr_review_threads(99).unwrap(), Vec::new());
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
    fn pr_state_open_when_not_merged_and_state_open() {
        let stdout = r#"{"state":"OPEN","merged":false}"#;
        assert_eq!(
            interpret_pr_state_output(Some(0), stdout, "").unwrap(),
            PrLifecycle::Open
        );
    }

    #[test]
    fn pr_state_merged_when_merged_true_even_if_state_closed() {
        let stdout = r#"{"state":"CLOSED","merged":true}"#;
        assert_eq!(
            interpret_pr_state_output(Some(0), stdout, "").unwrap(),
            PrLifecycle::Merged
        );
    }

    #[test]
    fn pr_state_closed_when_unmerged_and_state_closed() {
        let stdout = r#"{"state":"CLOSED","merged":false}"#;
        assert_eq!(
            interpret_pr_state_output(Some(0), stdout, "").unwrap(),
            PrLifecycle::Closed
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

        assert_eq!(fake.pr_reviews(42).unwrap(), reviews);
        assert_eq!(fake.pr_reviews_calls(), vec![42]);
    }

    #[test]
    fn fake_gh_cli_unconfigured_pr_reviews_returns_empty() {
        let fake = FakeGhCli::new();
        assert_eq!(fake.pr_reviews(99).unwrap(), Vec::new());
    }

    #[test]
    fn fake_gh_cli_returns_configured_pr_state_for_pr_number() {
        let fake = FakeGhCli::new().with_pr_state(42, Ok(PrLifecycle::Merged));

        assert_eq!(fake.pr_state(42).unwrap(), PrLifecycle::Merged);
        assert_eq!(fake.pr_state_calls(), vec![42]);
    }

    #[test]
    fn fake_gh_cli_unconfigured_pr_state_defaults_to_open() {
        let fake = FakeGhCli::new();
        assert_eq!(fake.pr_state(99).unwrap(), PrLifecycle::Open);
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
        assert_eq!(fake.pr_list().unwrap(), prs);
    }
}
