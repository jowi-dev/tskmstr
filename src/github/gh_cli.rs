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
use std::process::Command;

use thiserror::Error;

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
}

/// Fields requested from `gh pr view --json`; shared so the flag and the
/// [`PrInfo`] deserialization stay in lockstep.
const PR_VIEW_JSON_FIELDS: &str = "number,url,title,body,headRefName";

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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
