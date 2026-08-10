//! Git operations for the lane runner, ported from devtools' `~/devtools/work.ml`.
//!
//! [`GitOps`] is the trait callers depend on; [`ShellGitOps`] is the
//! `git`-shelling-out implementation used in production, following the same
//! trait+fake seam as [`crate::github::gh_cli::GhCli`]/[`ShellGhCli`]. Every
//! git invocation here takes an explicit working directory rather than
//! trusting the process's `cwd` — see `docs/plans/runner-port.md` §2: a lane
//! run's repo comes from lane config, not from wherever `tm work` happens to
//! be invoked from.
//!
//! # The `--no-track` incident
//!
//! Cutting a branch from a remote-tracking base (e.g. `origin/staging`)
//! without `--no-track` makes git set that base as the new branch's
//! upstream. With `push.default=tracking`, a later plain `git push` then
//! lands directly on the base branch instead of publishing the new branch —
//! this happened for real on 2026-08-05 (and once before, 2026-07-30,
//! per `work.ml`'s comments) and silently pushed straight to a shared base
//! branch. Both [`GitOps::provision_worktree`] (when cutting from an
//! explicit base) and [`GitOps::switch_new_branch`] (which always cuts from
//! a resolved base) pass `--no-track`. Do not remove it. The regression test
//! is [`worktree_add_args_includes_no_track_when_cutting_from_a_base`] and
//! [`switch_new_branch_args_always_includes_no_track`].

use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;

/// Errors that can occur while shelling out to `git`.
#[derive(Debug, Clone, Error)]
pub enum GitError {
    /// The `git` binary could not be spawned.
    #[error("failed to run `{command}`: {message}")]
    Spawn {
        /// The command that could not be spawned, e.g. `git worktree add`.
        command: String,
        /// The underlying spawn error message.
        message: String,
    },

    /// The command ran but exited with a failure.
    #[error("`{command}` failed (exit {exit_code:?}): {stderr}")]
    Command {
        /// The command that failed, e.g. `git worktree add`.
        command: String,
        /// The process exit code, if the process was not terminated by a signal.
        exit_code: Option<i32>,
        /// Captured stderr.
        stderr: String,
    },
}

/// Git operations the lane runner needs. Every method takes an explicit
/// working directory (usually a lane's configured `repo` path, or the
/// worktree path for a specific run) rather than trusting the caller's
/// `cwd`.
pub trait GitOps {
    /// The repository root for `dir`, mirroring `work.ml`'s `git_repo_root`
    /// (`git rev-parse --path-format=absolute --git-common-dir`, with a
    /// trailing `/.git` stripped). Uses `--git-common-dir` rather than
    /// `--git-dir` so that calling this from inside a linked worktree still
    /// returns the *main* repository's root, matching the OCaml behavior.
    fn repo_root(&self, dir: &Path) -> Result<PathBuf, GitError>;

    /// Whether `dir` is a linked worktree (as opposed to the main working
    /// tree of a repository), mirroring `work.ml`'s `is_worktree`: a linked
    /// worktree's git-dir contains a `commondir` file, the main repo's does
    /// not.
    fn is_worktree(&self, dir: &Path) -> Result<bool, GitError>;

    /// Whether `branch` exists as a local branch in the repository
    /// containing `dir` (`git show-ref --verify --quiet refs/heads/<branch>`).
    /// A non-zero exit (branch not found) is `Ok(false)`, not an error —
    /// only a failure to spawn `git` itself is an error.
    fn branch_exists_local(&self, dir: &Path, branch: &str) -> Result<bool, GitError>;

    /// Whether `branch` exists as a remote branch on `origin`
    /// (`git show-ref --verify --quiet refs/remotes/origin/<branch>`). Same
    /// not-found-is-not-an-error convention as [`GitOps::branch_exists_local`].
    fn branch_exists_remote(&self, dir: &Path, branch: &str) -> Result<bool, GitError>;

    /// Create the worktree at `wt_path` for `branch`, rooted at the
    /// repository containing `repo_dir` (`git worktree add ...`).
    ///
    /// If `branch` already exists (locally or on `origin`), attaches the
    /// worktree to it as-is. Otherwise cuts a new branch: from `from_base`
    /// if given (with `--no-track` — see the module-level "`--no-track`
    /// incident" section), or from the current `HEAD` if not.
    ///
    /// After the worktree is created, mirrors `work.ml`'s
    /// `provision_worktree`: if `<repo_dir>/.env.local` exists, symlinks it
    /// into the new worktree (`ln -sf`) — Axiom lane runs read `.env.local`
    /// for database URLs, so this is load-bearing, not cosmetic. Returns
    /// whether the link was created, so callers can print `work.ml`'s
    /// "Linked .env.local from main repo" message; this trait has no output
    /// sink of its own to print through.
    fn provision_worktree(
        &self,
        repo_dir: &Path,
        wt_path: &Path,
        branch: &str,
        from_base: Option<&str>,
    ) -> Result<bool, GitError>;

    /// Whether the working tree at `dir` has no uncommitted changes
    /// (`git status --porcelain` producing no output).
    fn status_is_clean(&self, dir: &Path) -> Result<bool, GitError>;

    /// Cut and switch to a new branch from `base` in the working tree at
    /// `dir` (`git switch -q --no-track -c <branch> <base>`), mirroring
    /// `work.ml`'s per-run branch cut in `run_lane`.
    ///
    /// Always passes `--no-track`: this is called every run to cut a fresh
    /// branch from the lane's resolved base, which is exactly the
    /// `push.default=tracking` scenario the `--no-track` incident (see
    /// module docs) was caused by. Do not make this conditional.
    fn switch_new_branch(&self, dir: &Path, branch: &str, base: &str) -> Result<(), GitError>;

    /// The repository's default remote branch (e.g. `origin/staging`),
    /// mirroring `work.ml`'s `default_base`
    /// (`git rev-parse --abbrev-ref origin/HEAD`).
    fn default_base(&self, dir: &Path) -> Result<String, GitError>;

    /// Remove the worktree at `wt_path`, mirroring `work.ml`'s
    /// `worktree_remove` (`git worktree remove <wt_path>`, run from
    /// `repo_dir`).
    ///
    /// `work.ml` doesn't pass `--force`: a worktree with uncommitted changes
    /// fails this call, and the caller is expected to surface git's own
    /// error (which suggests `--force`) rather than this trait forcing the
    /// removal itself.
    fn remove_worktree(&self, repo_dir: &Path, wt_path: &Path) -> Result<(), GitError>;

    /// Read a single git config value (`git config --get <key>`), used by
    /// [`crate::work::run`]'s branch-owner resolution chain (`j.branchOwner`,
    /// `github.user`). A missing key is `git config --get`'s normal exit
    /// code of 1, which is `Ok(None)` here, not an error — only a failure
    /// to spawn `git` itself is an error.
    fn config_get(&self, dir: &Path, key: &str) -> Result<Option<String>, GitError>;

    /// Fetch from `origin` in the working tree at `dir`
    /// (`git -C <dir> fetch --quiet origin`), mirroring `work.ml`'s
    /// `run_lane`: called right after provisioning-if-missing and before the
    /// dirty-worktree check, so the base ref (e.g. `origin/staging`) this
    /// run's branch is about to be cut from is current.
    ///
    /// `work.ml` ignores this command's exit status entirely (`let _ =
    /// Sys.command ...`) — a fetch can fail offline without making the run
    /// unviable. This trait surfaces the real `Result` rather than
    /// swallowing it; [`crate::work::run::run_lane_fg`] is the caller that
    /// decides to warn and continue rather than abort, matching `work.ml`'s
    /// tolerance but with a printed warning instead of silence.
    fn fetch_origin(&self, dir: &Path) -> Result<(), GitError>;
}

/// [`GitOps`] implementation that shells out to the real `git` binary.
pub struct ShellGitOps;

impl ShellGitOps {
    /// Create a new shell-backed git operations wrapper.
    pub fn new() -> Self {
        Self
    }
}

impl Default for ShellGitOps {
    fn default() -> Self {
        Self::new()
    }
}

/// Run `git -C <dir> <args>`, mapping a spawn failure to [`GitError::Spawn`]
/// tagged with `command` (a human-readable label, not the literal argv).
fn run_git(dir: &Path, args: &[String], command: &str) -> Result<std::process::Output, GitError> {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|err| GitError::Spawn {
            command: command.to_string(),
            message: err.to_string(),
        })
}

impl GitOps for ShellGitOps {
    fn repo_root(&self, dir: &Path) -> Result<PathBuf, GitError> {
        let output = run_git(
            dir,
            &[
                "rev-parse".to_string(),
                "--path-format=absolute".to_string(),
                "--git-common-dir".to_string(),
            ],
            "git rev-parse --git-common-dir",
        )?;

        interpret_repo_root_output(
            output.status.code(),
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        )
    }

    fn is_worktree(&self, dir: &Path) -> Result<bool, GitError> {
        let output = run_git(
            dir,
            &[
                "rev-parse".to_string(),
                "--path-format=absolute".to_string(),
                "--git-dir".to_string(),
            ],
            "git rev-parse --git-dir",
        )?;

        match output.status.code() {
            Some(0) => {
                let git_dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
                Ok(Path::new(&git_dir).join("commondir").exists())
            }
            Some(code) => Err(GitError::Command {
                command: "git rev-parse --git-dir".to_string(),
                exit_code: Some(code),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            }),
            None => Err(GitError::Command {
                command: "git rev-parse --git-dir".to_string(),
                exit_code: None,
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            }),
        }
    }

    fn branch_exists_local(&self, dir: &Path, branch: &str) -> Result<bool, GitError> {
        let output = run_git(
            dir,
            &show_ref_args(&format!("refs/heads/{branch}")),
            "git show-ref",
        )?;
        Ok(output.status.success())
    }

    fn branch_exists_remote(&self, dir: &Path, branch: &str) -> Result<bool, GitError> {
        let output = run_git(
            dir,
            &show_ref_args(&format!("refs/remotes/origin/{branch}")),
            "git show-ref",
        )?;
        Ok(output.status.success())
    }

    fn provision_worktree(
        &self,
        repo_dir: &Path,
        wt_path: &Path,
        branch: &str,
        from_base: Option<&str>,
    ) -> Result<bool, GitError> {
        let branch_exists = self.branch_exists_local(repo_dir, branch)?
            || self.branch_exists_remote(repo_dir, branch)?;
        let args = worktree_add_args(wt_path, branch, branch_exists, from_base);
        let output = run_git(repo_dir, &args, "git worktree add")?;

        interpret_success_or_command_error(
            "git worktree add",
            output.status.code(),
            &output.stderr,
        )?;

        Ok(link_env_local(repo_dir, wt_path))
    }

    fn status_is_clean(&self, dir: &Path) -> Result<bool, GitError> {
        let output = run_git(
            dir,
            &["status".to_string(), "--porcelain".to_string()],
            "git status --porcelain",
        )?;

        match output.status.code() {
            Some(0) => Ok(String::from_utf8_lossy(&output.stdout).trim().is_empty()),
            Some(code) => Err(GitError::Command {
                command: "git status --porcelain".to_string(),
                exit_code: Some(code),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            }),
            None => Err(GitError::Command {
                command: "git status --porcelain".to_string(),
                exit_code: None,
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            }),
        }
    }

    fn switch_new_branch(&self, dir: &Path, branch: &str, base: &str) -> Result<(), GitError> {
        let args = switch_new_branch_args(branch, base);
        let output = run_git(dir, &args, "git switch")?;

        interpret_success_or_command_error("git switch", output.status.code(), &output.stderr)
    }

    fn default_base(&self, dir: &Path) -> Result<String, GitError> {
        let output = run_git(
            dir,
            &[
                "rev-parse".to_string(),
                "--abbrev-ref".to_string(),
                "origin/HEAD".to_string(),
            ],
            "git rev-parse --abbrev-ref origin/HEAD",
        )?;

        match output.status.code() {
            Some(0) => {
                let base = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if base.is_empty() {
                    Err(GitError::Command {
                        command: "git rev-parse --abbrev-ref origin/HEAD".to_string(),
                        exit_code: Some(0),
                        stderr: "empty output".to_string(),
                    })
                } else {
                    Ok(base)
                }
            }
            Some(code) => Err(GitError::Command {
                command: "git rev-parse --abbrev-ref origin/HEAD".to_string(),
                exit_code: Some(code),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            }),
            None => Err(GitError::Command {
                command: "git rev-parse --abbrev-ref origin/HEAD".to_string(),
                exit_code: None,
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            }),
        }
    }

    fn remove_worktree(&self, repo_dir: &Path, wt_path: &Path) -> Result<(), GitError> {
        let output = run_git(
            repo_dir,
            &remove_worktree_args(wt_path),
            "git worktree remove",
        )?;
        interpret_success_or_command_error(
            "git worktree remove",
            output.status.code(),
            &output.stderr,
        )
    }

    fn config_get(&self, dir: &Path, key: &str) -> Result<Option<String>, GitError> {
        let output = run_git(
            dir,
            &["config".to_string(), "--get".to_string(), key.to_string()],
            "git config --get",
        )?;

        match output.status.code() {
            Some(0) => {
                let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
                Ok(if value.is_empty() { None } else { Some(value) })
            }
            // A missing key is git config --get's normal exit code 1, not a
            // failure to report.
            _ => Ok(None),
        }
    }

    fn fetch_origin(&self, dir: &Path) -> Result<(), GitError> {
        let output = run_git(dir, &fetch_origin_args(), "git fetch --quiet origin")?;
        interpret_success_or_command_error(
            "git fetch --quiet origin",
            output.status.code(),
            &output.stderr,
        )
    }
}

/// Symlink `<repo_dir>/.env.local` into `<wt_path>/.env.local` if the former
/// exists, mirroring `work.ml`'s `provision_worktree` (`ln -sf`). Replaces
/// any existing file/link at the destination first, matching `ln -sf`'s
/// overwrite semantics. Returns whether a link was created (i.e. whether
/// `.env.local` existed in `repo_dir` at all) so callers can print
/// `work.ml`'s "Linked .env.local from main repo" message.
fn link_env_local(repo_dir: &Path, wt_path: &Path) -> bool {
    let src = repo_dir.join(".env.local");
    if !src.exists() {
        return false;
    }
    let dest = wt_path.join(".env.local");
    let _ = std::fs::remove_file(&dest);
    std::os::unix::fs::symlink(&src, &dest).is_ok()
}

/// Build the argument list for `git fetch --quiet origin`, mirroring
/// `work.ml`'s `run_lane` fetch (`git -C '<wt_path>' fetch --quiet origin`).
fn fetch_origin_args() -> Vec<String> {
    vec![
        "fetch".to_string(),
        "--quiet".to_string(),
        "origin".to_string(),
    ]
}

/// Build the argument list for `git show-ref --verify --quiet <ref>`, shared
/// by [`GitOps::branch_exists_local`] and [`GitOps::branch_exists_remote`].
fn show_ref_args(git_ref: &str) -> Vec<String> {
    vec![
        "show-ref".to_string(),
        "--verify".to_string(),
        "--quiet".to_string(),
        git_ref.to_string(),
    ]
}

/// Build the argument list for `git worktree add ...`, mirroring `work.ml`'s
/// `provision_worktree` command construction:
///
/// ```ocaml
/// if branch_exists_local branch || branch_exists_remote branch then
///   sprintf "git worktree add '%s' '%s'" wt_path branch
/// else
///   match from_opt with
///   | Some base -> sprintf "git worktree add --no-track -b '%s' '%s' '%s'" branch wt_path base
///   | None -> sprintf "git worktree add -b '%s' '%s'" branch wt_path
/// ```
///
/// `--no-track` is present if and only if `branch_exists` is `false` and
/// `from_base` is `Some` — cutting a brand-new branch from an explicit base.
/// See the module-level "`--no-track` incident" docs for why this matters;
/// do not drop it.
fn worktree_add_args(
    wt_path: &Path,
    branch: &str,
    branch_exists: bool,
    from_base: Option<&str>,
) -> Vec<String> {
    let mut args = vec!["worktree".to_string(), "add".to_string()];

    if branch_exists {
        args.push(wt_path.display().to_string());
        args.push(branch.to_string());
        return args;
    }

    if from_base.is_some() {
        args.push("--no-track".to_string());
    }
    args.push("-b".to_string());
    args.push(branch.to_string());
    args.push(wt_path.display().to_string());
    if let Some(base) = from_base {
        args.push(base.to_string());
    }
    args
}

/// Build the argument list for `git worktree remove <wt_path>`, mirroring
/// `work.ml`'s `worktree_remove`. No `--force`: see [`GitOps::remove_worktree`].
fn remove_worktree_args(wt_path: &Path) -> Vec<String> {
    vec![
        "worktree".to_string(),
        "remove".to_string(),
        wt_path.display().to_string(),
    ]
}

/// Build the argument list for `git switch -q --no-track -c <branch> <base>`,
/// mirroring `work.ml`'s per-run branch cut in `run_lane`. `--no-track` is
/// unconditional here — see [`GitOps::switch_new_branch`]'s doc comment and
/// the module-level "`--no-track` incident" docs.
fn switch_new_branch_args(branch: &str, base: &str) -> Vec<String> {
    vec![
        "switch".to_string(),
        "-q".to_string(),
        "--no-track".to_string(),
        "-c".to_string(),
        branch.to_string(),
        base.to_string(),
    ]
}

/// Interpret the result of a `git rev-parse --git-common-dir` invocation,
/// stripping a trailing `/.git` the same way `work.ml`'s `git_repo_root`
/// does. Pure over the exit code and captured stdout/stderr so parsing can
/// be unit tested without shelling out.
fn interpret_repo_root_output(
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
) -> Result<PathBuf, GitError> {
    match exit_code {
        Some(0) => {
            let git_dir = Path::new(stdout.trim());
            let root = if git_dir.file_name().and_then(|n| n.to_str()) == Some(".git") {
                git_dir
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| git_dir.to_path_buf())
            } else {
                git_dir.to_path_buf()
            };
            Ok(root)
        }
        Some(code) => Err(GitError::Command {
            command: "git rev-parse --git-common-dir".to_string(),
            exit_code: Some(code),
            stderr: stderr.trim().to_string(),
        }),
        None => Err(GitError::Command {
            command: "git rev-parse --git-common-dir".to_string(),
            exit_code: None,
            stderr: stderr.trim().to_string(),
        }),
    }
}

/// Shared success/failure interpretation for commands whose output carries
/// no information beyond "it worked", mirroring the same helper in
/// `crate::github::gh_cli`.
fn interpret_success_or_command_error(
    command: &str,
    exit_code: Option<i32>,
    stderr: &[u8],
) -> Result<(), GitError> {
    match exit_code {
        Some(0) => Ok(()),
        Some(code) => Err(GitError::Command {
            command: command.to_string(),
            exit_code: Some(code),
            stderr: String::from_utf8_lossy(stderr).trim().to_string(),
        }),
        None => Err(GitError::Command {
            command: command.to_string(),
            exit_code: None,
            stderr: String::from_utf8_lossy(stderr).trim().to_string(),
        }),
    }
}

/// Recorded call to [`GitOps::provision_worktree`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionWorktreeCall {
    /// `repo_dir` argument.
    pub repo_dir: PathBuf,
    /// `wt_path` argument.
    pub wt_path: PathBuf,
    /// `branch` argument.
    pub branch: String,
    /// `from_base` argument.
    pub from_base: Option<String>,
}

/// Recorded call to [`GitOps::switch_new_branch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchNewBranchCall {
    /// `dir` argument.
    pub dir: PathBuf,
    /// `branch` argument.
    pub branch: String,
    /// `base` argument.
    pub base: String,
}

/// A [`GitOps`] test double: returns canned results and records calls made
/// against it, for use by tests that don't want to shell out to a real
/// `git`. Follows the same pattern as
/// [`crate::github::gh_cli::FakeGhCli`].
///
/// This is a plain public struct (not `#[cfg(test)]`-gated) so other test
/// code in the crate can depend on it directly.
pub struct FakeGitOps {
    repo_root_result: std::cell::RefCell<Result<PathBuf, GitError>>,
    is_worktree_result: std::cell::RefCell<Result<bool, GitError>>,
    branch_exists_local_result: std::cell::RefCell<Result<bool, GitError>>,
    branch_exists_remote_result: std::cell::RefCell<Result<bool, GitError>>,
    provision_worktree_result: std::cell::RefCell<Result<(), GitError>>,
    status_is_clean_result: std::cell::RefCell<Result<bool, GitError>>,
    switch_new_branch_result: std::cell::RefCell<Result<(), GitError>>,
    default_base_result: std::cell::RefCell<Result<String, GitError>>,
    remove_worktree_result: std::cell::RefCell<Result<(), GitError>>,
    fetch_origin_result: std::cell::RefCell<Result<(), GitError>>,

    branch_exists_local_calls: std::cell::RefCell<Vec<(PathBuf, String)>>,
    branch_exists_remote_calls: std::cell::RefCell<Vec<(PathBuf, String)>>,
    provision_worktree_calls: std::cell::RefCell<Vec<ProvisionWorktreeCall>>,
    switch_new_branch_calls: std::cell::RefCell<Vec<SwitchNewBranchCall>>,
    remove_worktree_calls: std::cell::RefCell<Vec<(PathBuf, PathBuf)>>,
    fetch_origin_calls: std::cell::RefCell<Vec<PathBuf>>,
    config_values: std::cell::RefCell<std::collections::HashMap<String, String>>,
    /// Branch names that report as existing (both `branch_exists_local` and
    /// `branch_exists_remote`) regardless of those methods' blanket
    /// `_result` overrides — see [`Self::with_existing_branches`]. Used by
    /// [`crate::work::run`]'s branch-name-collision tests, where different
    /// candidate names (e.g. a slug and its `-2` suffix) need different
    /// existence answers in the same test.
    existing_branches: std::cell::RefCell<std::collections::HashSet<String>>,
    /// Method names, in call order, across every traced method — currently
    /// just [`GitOps::fetch_origin`] and [`GitOps::switch_new_branch`], the
    /// pair whose relative order [`crate::work::run::run_lane_fg`]'s tests
    /// need to assert on (fetch must happen before the branch is cut).
    call_log: std::cell::RefCell<Vec<&'static str>>,
}

impl Default for FakeGitOps {
    /// A fake reporting a clean, non-worktree repo with no existing
    /// branches and `origin/main` as the default base; overrides via the
    /// `with_*` builders as needed.
    fn default() -> Self {
        Self {
            repo_root_result: std::cell::RefCell::new(Ok(PathBuf::from("/repo"))),
            is_worktree_result: std::cell::RefCell::new(Ok(false)),
            branch_exists_local_result: std::cell::RefCell::new(Ok(false)),
            branch_exists_remote_result: std::cell::RefCell::new(Ok(false)),
            provision_worktree_result: std::cell::RefCell::new(Ok(())),
            status_is_clean_result: std::cell::RefCell::new(Ok(true)),
            switch_new_branch_result: std::cell::RefCell::new(Ok(())),
            default_base_result: std::cell::RefCell::new(Ok("origin/main".to_string())),
            remove_worktree_result: std::cell::RefCell::new(Ok(())),
            fetch_origin_result: std::cell::RefCell::new(Ok(())),
            branch_exists_local_calls: std::cell::RefCell::new(Vec::new()),
            branch_exists_remote_calls: std::cell::RefCell::new(Vec::new()),
            provision_worktree_calls: std::cell::RefCell::new(Vec::new()),
            switch_new_branch_calls: std::cell::RefCell::new(Vec::new()),
            remove_worktree_calls: std::cell::RefCell::new(Vec::new()),
            fetch_origin_calls: std::cell::RefCell::new(Vec::new()),
            config_values: std::cell::RefCell::new(std::collections::HashMap::new()),
            existing_branches: std::cell::RefCell::new(std::collections::HashSet::new()),
            call_log: std::cell::RefCell::new(Vec::new()),
        }
    }
}

impl FakeGitOps {
    /// Create a fake with the default canned results (see [`Default`]).
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the result `repo_root` will return.
    pub fn with_repo_root(self, result: Result<PathBuf, GitError>) -> Self {
        *self.repo_root_result.borrow_mut() = result;
        self
    }

    /// Set the result `is_worktree` will return.
    pub fn with_is_worktree(self, result: Result<bool, GitError>) -> Self {
        *self.is_worktree_result.borrow_mut() = result;
        self
    }

    /// Set the result `branch_exists_local` will return.
    pub fn with_branch_exists_local(self, result: Result<bool, GitError>) -> Self {
        *self.branch_exists_local_result.borrow_mut() = result;
        self
    }

    /// Set the result `branch_exists_remote` will return.
    pub fn with_branch_exists_remote(self, result: Result<bool, GitError>) -> Self {
        *self.branch_exists_remote_result.borrow_mut() = result;
        self
    }

    /// Mark specific branch names as already existing, for both
    /// `branch_exists_local` and `branch_exists_remote` — used by
    /// branch-name-collision tests (see
    /// [`crate::work::naming::resolve_branch_collision`]) that need
    /// different candidate names to report different existence answers in
    /// the same test, which the blanket `with_branch_exists_local`/
    /// `with_branch_exists_remote` overrides can't express. A name in this
    /// set always reports as existing regardless of those overrides; a name
    /// not in it falls back to whatever they (or their `Ok(false)` default)
    /// say.
    pub fn with_existing_branches(
        self,
        names: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.existing_branches
            .borrow_mut()
            .extend(names.into_iter().map(Into::into));
        self
    }

    /// Set the result `provision_worktree` will return.
    pub fn with_provision_worktree_result(self, result: Result<(), GitError>) -> Self {
        *self.provision_worktree_result.borrow_mut() = result;
        self
    }

    /// Set the result `status_is_clean` will return.
    pub fn with_status_is_clean(self, result: Result<bool, GitError>) -> Self {
        *self.status_is_clean_result.borrow_mut() = result;
        self
    }

    /// Set the result `switch_new_branch` will return.
    pub fn with_switch_new_branch_result(self, result: Result<(), GitError>) -> Self {
        *self.switch_new_branch_result.borrow_mut() = result;
        self
    }

    /// Set the result `default_base` will return.
    pub fn with_default_base(self, result: Result<String, GitError>) -> Self {
        *self.default_base_result.borrow_mut() = result;
        self
    }

    /// Set the result `remove_worktree` will return.
    pub fn with_remove_worktree_result(self, result: Result<(), GitError>) -> Self {
        *self.remove_worktree_result.borrow_mut() = result;
        self
    }

    /// Set the result `fetch_origin` will return.
    pub fn with_fetch_origin_result(self, result: Result<(), GitError>) -> Self {
        *self.fetch_origin_result.borrow_mut() = result;
        self
    }

    /// The `dir` arguments passed to `fetch_origin`, in call order.
    pub fn fetch_origin_calls(&self) -> Vec<PathBuf> {
        self.fetch_origin_calls.borrow().clone()
    }

    /// Method names, in call order, across every traced method. See the
    /// `call_log` field doc for which methods are traced.
    pub fn call_log(&self) -> Vec<&'static str> {
        self.call_log.borrow().clone()
    }

    /// The `(repo_dir, wt_path)` pairs passed to `remove_worktree`, in call order.
    pub fn remove_worktree_calls(&self) -> Vec<(PathBuf, PathBuf)> {
        self.remove_worktree_calls.borrow().clone()
    }

    /// The `(dir, branch)` pairs passed to `branch_exists_local`, in call order.
    pub fn branch_exists_local_calls(&self) -> Vec<(PathBuf, String)> {
        self.branch_exists_local_calls.borrow().clone()
    }

    /// The `(dir, branch)` pairs passed to `branch_exists_remote`, in call order.
    pub fn branch_exists_remote_calls(&self) -> Vec<(PathBuf, String)> {
        self.branch_exists_remote_calls.borrow().clone()
    }

    /// The calls made to `provision_worktree`, in call order.
    pub fn provision_worktree_calls(&self) -> Vec<ProvisionWorktreeCall> {
        self.provision_worktree_calls.borrow().clone()
    }

    /// The calls made to `switch_new_branch`, in call order.
    pub fn switch_new_branch_calls(&self) -> Vec<SwitchNewBranchCall> {
        self.switch_new_branch_calls.borrow().clone()
    }

    /// Configure `config_get(_, key)` to return `Some(value)`. Keys with no
    /// configured value return `Ok(None)`, mirroring a real unset git config
    /// key.
    pub fn with_config_value(self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.config_values
            .borrow_mut()
            .insert(key.into(), value.into());
        self
    }
}

impl GitOps for FakeGitOps {
    fn repo_root(&self, _dir: &Path) -> Result<PathBuf, GitError> {
        self.repo_root_result.borrow().clone()
    }

    fn is_worktree(&self, _dir: &Path) -> Result<bool, GitError> {
        self.is_worktree_result.borrow().clone()
    }

    fn branch_exists_local(&self, dir: &Path, branch: &str) -> Result<bool, GitError> {
        self.branch_exists_local_calls
            .borrow_mut()
            .push((dir.to_path_buf(), branch.to_string()));
        if self.existing_branches.borrow().contains(branch) {
            return Ok(true);
        }
        self.branch_exists_local_result.borrow().clone()
    }

    fn branch_exists_remote(&self, dir: &Path, branch: &str) -> Result<bool, GitError> {
        self.branch_exists_remote_calls
            .borrow_mut()
            .push((dir.to_path_buf(), branch.to_string()));
        if self.existing_branches.borrow().contains(branch) {
            return Ok(true);
        }
        self.branch_exists_remote_result.borrow().clone()
    }

    fn provision_worktree(
        &self,
        repo_dir: &Path,
        wt_path: &Path,
        branch: &str,
        from_base: Option<&str>,
    ) -> Result<bool, GitError> {
        self.provision_worktree_calls
            .borrow_mut()
            .push(ProvisionWorktreeCall {
                repo_dir: repo_dir.to_path_buf(),
                wt_path: wt_path.to_path_buf(),
                branch: branch.to_string(),
                from_base: from_base.map(str::to_string),
            });
        let result = self.provision_worktree_result.borrow().clone();
        // On a configured success, actually create `wt_path` on disk. A
        // real `git worktree add` does this; callers layered on top of
        // this fake (e.g. `cli::work::new`, which checks the directory
        // exists before starting a session in it) need that same
        // real-filesystem effect to be exercisable in tests without
        // shelling out to `git`.
        match result {
            Ok(()) => {
                let _ = std::fs::create_dir_all(wt_path);
                Ok(link_env_local(repo_dir, wt_path))
            }
            Err(err) => Err(err),
        }
    }

    fn status_is_clean(&self, _dir: &Path) -> Result<bool, GitError> {
        self.status_is_clean_result.borrow().clone()
    }

    fn switch_new_branch(&self, dir: &Path, branch: &str, base: &str) -> Result<(), GitError> {
        self.call_log.borrow_mut().push("switch_new_branch");
        self.switch_new_branch_calls
            .borrow_mut()
            .push(SwitchNewBranchCall {
                dir: dir.to_path_buf(),
                branch: branch.to_string(),
                base: base.to_string(),
            });
        self.switch_new_branch_result.borrow().clone()
    }

    fn default_base(&self, _dir: &Path) -> Result<String, GitError> {
        self.default_base_result.borrow().clone()
    }

    fn remove_worktree(&self, repo_dir: &Path, wt_path: &Path) -> Result<(), GitError> {
        self.remove_worktree_calls
            .borrow_mut()
            .push((repo_dir.to_path_buf(), wt_path.to_path_buf()));
        self.remove_worktree_result.borrow().clone()
    }

    fn config_get(&self, _dir: &Path, key: &str) -> Result<Option<String>, GitError> {
        Ok(self.config_values.borrow().get(key).cloned())
    }

    fn fetch_origin(&self, dir: &Path) -> Result<(), GitError> {
        self.call_log.borrow_mut().push("fetch_origin");
        self.fetch_origin_calls.borrow_mut().push(dir.to_path_buf());
        self.fetch_origin_result.borrow().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;
    use tempfile::TempDir;

    // --- pure arg-builder tests ---

    #[test]
    fn worktree_add_args_includes_no_track_when_cutting_from_a_base() {
        // Regression test for the 2026-08-05 push-to-base incident: cutting
        // a brand-new branch from an explicit remote base MUST pass
        // --no-track, or push.default=tracking sends a later `git push`
        // straight to the base branch. Do not remove this assertion.
        let args = worktree_add_args(
            Path::new("/Worktrees/axiom/lane"),
            "jowi-dev/lane-20260806-090503",
            false,
            Some("origin/staging"),
        );
        assert!(
            args.contains(&"--no-track".to_string()),
            "expected --no-track in worktree add args, got: {args:?}"
        );
        assert_eq!(
            args,
            vec![
                "worktree",
                "add",
                "--no-track",
                "-b",
                "jowi-dev/lane-20260806-090503",
                "/Worktrees/axiom/lane",
                "origin/staging",
            ]
        );
    }

    #[test]
    fn worktree_add_args_omits_no_track_when_no_base_given() {
        // Cutting a new branch from the current HEAD (no explicit base)
        // never sets an upstream in the first place, so --no-track is
        // unnecessary — matches work.ml's `None -> ... "-b" ...` arm.
        let args = worktree_add_args(Path::new("/Worktrees/axiom/lane"), "lane", false, None);
        assert!(!args.contains(&"--no-track".to_string()));
        assert_eq!(
            args,
            vec!["worktree", "add", "-b", "lane", "/Worktrees/axiom/lane"]
        );
    }

    #[test]
    fn worktree_add_args_attaches_to_existing_branch_without_dash_b() {
        let args = worktree_add_args(
            Path::new("/Worktrees/axiom/lane"),
            "existing-branch",
            true,
            Some("origin/staging"),
        );
        assert_eq!(
            args,
            vec![
                "worktree",
                "add",
                "/Worktrees/axiom/lane",
                "existing-branch"
            ]
        );
    }

    #[test]
    fn switch_new_branch_args_always_includes_no_track() {
        // Regression test for the same --no-track incident: run_lane cuts a
        // fresh branch from the resolved base on every run, so this must be
        // unconditional (there is no branch_exists check here at all).
        let args = switch_new_branch_args("jowi-dev/lane-20260806-090503", "origin/staging");
        assert!(
            args.contains(&"--no-track".to_string()),
            "expected --no-track in switch args, got: {args:?}"
        );
        assert_eq!(
            args,
            vec![
                "switch",
                "-q",
                "--no-track",
                "-c",
                "jowi-dev/lane-20260806-090503",
                "origin/staging",
            ]
        );
    }

    #[test]
    fn remove_worktree_args_match_work_ml() {
        let args = remove_worktree_args(Path::new("/Worktrees/axiom/lane"));
        assert_eq!(args, vec!["worktree", "remove", "/Worktrees/axiom/lane"]);
    }

    #[test]
    fn show_ref_args_builds_verify_quiet_ref() {
        assert_eq!(
            show_ref_args("refs/heads/main"),
            vec!["show-ref", "--verify", "--quiet", "refs/heads/main"]
        );
    }

    #[test]
    fn fetch_origin_args_match_work_ml() {
        assert_eq!(fetch_origin_args(), vec!["fetch", "--quiet", "origin"]);
    }

    // --- output-interpretation tests ---

    #[test]
    fn repo_root_strips_trailing_dot_git() {
        let root =
            interpret_repo_root_output(Some(0), "/Users/jowi/Projects/axiom/.git\n", "").unwrap();
        assert_eq!(root, PathBuf::from("/Users/jowi/Projects/axiom"));
    }

    #[test]
    fn repo_root_bare_git_common_dir_is_kept_as_is() {
        // A bare/linked-worktree git-common-dir that isn't itself named
        // ".git" (unusual, but not impossible) is returned unchanged,
        // mirroring work.ml's basename check.
        let root = interpret_repo_root_output(Some(0), "/Users/jowi/bare-repo\n", "").unwrap();
        assert_eq!(root, PathBuf::from("/Users/jowi/bare-repo"));
    }

    #[test]
    fn repo_root_failure_is_a_command_error() {
        let err =
            interpret_repo_root_output(Some(128), "", "fatal: not a git repository").unwrap_err();
        match err {
            GitError::Command { stderr, .. } => assert!(stderr.contains("not a git repository")),
            other => panic!("expected Command error, got {other:?}"),
        }
    }

    #[test]
    fn success_or_command_error_success_is_ok() {
        interpret_success_or_command_error("git switch", Some(0), b"").unwrap();
    }

    #[test]
    fn success_or_command_error_failure_is_a_command_error() {
        let err =
            interpret_success_or_command_error("git switch", Some(1), b"fatal: branch exists")
                .unwrap_err();
        match err {
            GitError::Command {
                command, stderr, ..
            } => {
                assert_eq!(command, "git switch");
                assert!(stderr.contains("branch exists"));
            }
            other => panic!("expected Command error, got {other:?}"),
        }
    }

    // --- FakeGitOps tests ---

    #[test]
    fn fake_git_ops_records_provision_worktree_calls() {
        let fake = FakeGitOps::new();
        fake.provision_worktree(
            Path::new("/repo"),
            Path::new("/Worktrees/repo/lane"),
            "jowi-dev/lane-1",
            Some("origin/staging"),
        )
        .unwrap();

        assert_eq!(
            fake.provision_worktree_calls(),
            vec![ProvisionWorktreeCall {
                repo_dir: PathBuf::from("/repo"),
                wt_path: PathBuf::from("/Worktrees/repo/lane"),
                branch: "jowi-dev/lane-1".to_string(),
                from_base: Some("origin/staging".to_string()),
            }]
        );
    }

    #[test]
    fn fake_git_ops_records_switch_new_branch_calls() {
        let fake = FakeGitOps::new();
        fake.switch_new_branch(Path::new("/wt"), "jowi-dev/lane-1", "origin/staging")
            .unwrap();

        assert_eq!(
            fake.switch_new_branch_calls(),
            vec![SwitchNewBranchCall {
                dir: PathBuf::from("/wt"),
                branch: "jowi-dev/lane-1".to_string(),
                base: "origin/staging".to_string(),
            }]
        );
    }

    #[test]
    fn fake_git_ops_returns_configured_default_base() {
        let fake = FakeGitOps::new().with_default_base(Ok("origin/staging".to_string()));
        assert_eq!(
            fake.default_base(Path::new("/repo")).unwrap(),
            "origin/staging"
        );
    }

    #[test]
    fn fake_git_ops_config_get_returns_none_for_unconfigured_key() {
        let fake = FakeGitOps::new();
        assert_eq!(
            fake.config_get(Path::new("/repo"), "j.branchOwner")
                .unwrap(),
            None
        );
    }

    #[test]
    fn fake_git_ops_config_get_returns_configured_value() {
        let fake = FakeGitOps::new().with_config_value("j.branchOwner", "jowi-dev");
        assert_eq!(
            fake.config_get(Path::new("/repo"), "j.branchOwner")
                .unwrap(),
            Some("jowi-dev".to_string())
        );
    }

    #[test]
    fn fake_git_ops_returns_configured_error() {
        let fake = FakeGitOps::new().with_status_is_clean(Err(GitError::Command {
            command: "git status".to_string(),
            exit_code: Some(1),
            stderr: "boom".to_string(),
        }));
        assert!(fake.status_is_clean(Path::new("/wt")).is_err());
    }

    #[test]
    fn fake_git_ops_records_fetch_origin_calls() {
        let fake = FakeGitOps::new();
        fake.fetch_origin(Path::new("/wt")).unwrap();

        assert_eq!(fake.fetch_origin_calls(), vec![PathBuf::from("/wt")]);
    }

    #[test]
    fn fake_git_ops_fetch_origin_returns_configured_error() {
        let fake = FakeGitOps::new().with_fetch_origin_result(Err(GitError::Command {
            command: "git fetch".to_string(),
            exit_code: Some(1),
            stderr: "could not resolve host".to_string(),
        }));
        assert!(fake.fetch_origin(Path::new("/wt")).is_err());
    }

    #[test]
    fn fake_git_ops_call_log_records_fetch_origin_before_switch_new_branch() {
        let fake = FakeGitOps::new();
        fake.fetch_origin(Path::new("/wt")).unwrap();
        fake.switch_new_branch(Path::new("/wt"), "branch", "origin/main")
            .unwrap();

        assert_eq!(fake.call_log(), vec!["fetch_origin", "switch_new_branch"]);
    }

    #[test]
    fn fake_git_ops_provision_worktree_links_env_local_when_present_in_repo_dir() {
        let tmp = TempDir::new().unwrap();
        let repo_dir = tmp.path().join("repo");
        let wt_path = tmp.path().join("wt");
        std::fs::create_dir_all(&repo_dir).unwrap();
        std::fs::write(repo_dir.join(".env.local"), "DATABASE_URL=postgres://\n").unwrap();

        let fake = FakeGitOps::new();
        let linked = fake
            .provision_worktree(&repo_dir, &wt_path, "lane", None)
            .unwrap();

        assert!(linked);
        assert!(wt_path.join(".env.local").is_symlink());
    }

    #[test]
    fn fake_git_ops_provision_worktree_reports_no_link_when_env_local_absent() {
        let tmp = TempDir::new().unwrap();
        let repo_dir = tmp.path().join("repo");
        let wt_path = tmp.path().join("wt");
        std::fs::create_dir_all(&repo_dir).unwrap();

        let fake = FakeGitOps::new();
        let linked = fake
            .provision_worktree(&repo_dir, &wt_path, "lane", None)
            .unwrap();

        assert!(!linked);
        assert!(!wt_path.join(".env.local").exists());
    }

    // --- ShellGitOps integration tests against a real temp git repo ---

    fn git_init(dir: &Path) {
        let status = StdCommand::new("git")
            .arg("-C")
            .arg(dir)
            .args(["init", "-q", "-b", "main"])
            .status()
            .expect("git init should run");
        assert!(status.success());
        StdCommand::new("git")
            .arg("-C")
            .arg(dir)
            .args(["config", "user.email", "test@example.com"])
            .status()
            .unwrap();
        StdCommand::new("git")
            .arg("-C")
            .arg(dir)
            .args(["config", "user.name", "Test"])
            .status()
            .unwrap();
        std::fs::write(dir.join("README.md"), "hello\n").unwrap();
        StdCommand::new("git")
            .arg("-C")
            .arg(dir)
            .args(["add", "."])
            .status()
            .unwrap();
        StdCommand::new("git")
            .arg("-C")
            .arg(dir)
            .args(["commit", "-q", "-m", "init"])
            .status()
            .unwrap();
    }

    #[test]
    fn shell_git_ops_repo_root_returns_repo_path() {
        let tmp = TempDir::new().unwrap();
        git_init(tmp.path());

        let ops = ShellGitOps::new();
        let root = ops.repo_root(tmp.path()).unwrap();
        // Canonicalize both sides: tempdir paths on macOS often resolve
        // through a /private symlink, which `git` itself resolves too.
        assert_eq!(
            root.canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn shell_git_ops_is_worktree_false_for_main_repo() {
        let tmp = TempDir::new().unwrap();
        git_init(tmp.path());

        let ops = ShellGitOps::new();
        assert!(!ops.is_worktree(tmp.path()).unwrap());
    }

    #[test]
    fn shell_git_ops_status_is_clean_reflects_working_tree_state() {
        let tmp = TempDir::new().unwrap();
        git_init(tmp.path());

        let ops = ShellGitOps::new();
        assert!(ops.status_is_clean(tmp.path()).unwrap());

        std::fs::write(tmp.path().join("dirty.txt"), "uncommitted\n").unwrap();
        assert!(!ops.status_is_clean(tmp.path()).unwrap());
    }

    #[test]
    fn shell_git_ops_branch_exists_local_reflects_created_branches() {
        let tmp = TempDir::new().unwrap();
        git_init(tmp.path());

        let ops = ShellGitOps::new();
        assert!(!ops.branch_exists_local(tmp.path(), "feature").unwrap());

        StdCommand::new("git")
            .arg("-C")
            .arg(tmp.path())
            .args(["branch", "feature"])
            .status()
            .unwrap();

        assert!(ops.branch_exists_local(tmp.path(), "feature").unwrap());
    }

    #[test]
    fn shell_git_ops_provision_worktree_creates_worktree_and_branch() {
        let tmp = TempDir::new().unwrap();
        git_init(tmp.path());
        let wt_path = tmp.path().join("wt");

        let ops = ShellGitOps::new();
        ops.provision_worktree(tmp.path(), &wt_path, "new-lane-branch", None)
            .unwrap();

        assert!(wt_path.join("README.md").exists());
        assert!(
            ops.branch_exists_local(tmp.path(), "new-lane-branch")
                .unwrap()
        );
        assert!(ops.is_worktree(&wt_path).unwrap());
    }

    #[test]
    fn shell_git_ops_provision_worktree_from_base_does_not_set_upstream() {
        // End-to-end guard for the --no-track incident: after provisioning
        // from an explicit base, the new branch must have no upstream
        // configured, or a later `git push` with push.default=tracking
        // would land on that base.
        let tmp = TempDir::new().unwrap();
        git_init(tmp.path());
        let wt_path = tmp.path().join("wt");

        let ops = ShellGitOps::new();
        ops.provision_worktree(tmp.path(), &wt_path, "from-base-branch", Some("main"))
            .unwrap();

        let output = StdCommand::new("git")
            .arg("-C")
            .arg(&wt_path)
            .args(["rev-parse", "--abbrev-ref", "from-base-branch@{upstream}"])
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "expected no upstream to be configured for a --no-track branch"
        );
    }

    #[test]
    fn shell_git_ops_provision_worktree_links_env_local_from_main_repo() {
        let tmp = TempDir::new().unwrap();
        git_init(tmp.path());
        std::fs::write(tmp.path().join(".env.local"), "DATABASE_URL=postgres://\n").unwrap();
        let wt_path = tmp.path().join("wt");

        let ops = ShellGitOps::new();
        let linked = ops
            .provision_worktree(tmp.path(), &wt_path, "env-local-branch", None)
            .unwrap();

        assert!(linked);
        let link = wt_path.join(".env.local");
        assert!(link.is_symlink());
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            tmp.path().join(".env.local")
        );
        assert_eq!(
            std::fs::read_to_string(&link).unwrap(),
            "DATABASE_URL=postgres://\n"
        );
    }

    #[test]
    fn shell_git_ops_provision_worktree_reports_no_link_when_env_local_absent() {
        let tmp = TempDir::new().unwrap();
        git_init(tmp.path());
        let wt_path = tmp.path().join("wt");

        let ops = ShellGitOps::new();
        let linked = ops
            .provision_worktree(tmp.path(), &wt_path, "no-env-local-branch", None)
            .unwrap();

        assert!(!linked);
        assert!(!wt_path.join(".env.local").exists());
    }

    #[test]
    fn shell_git_ops_fetch_origin_succeeds_against_real_remote() {
        let tmp = TempDir::new().unwrap();
        let origin = tmp.path().join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        git_init(&origin);

        let clone = tmp.path().join("clone");
        let status = StdCommand::new("git")
            .args(["clone", "-q"])
            .arg(&origin)
            .arg(&clone)
            .status()
            .unwrap();
        assert!(status.success());

        let ops = ShellGitOps::new();
        ops.fetch_origin(&clone).unwrap();
    }

    #[test]
    fn shell_git_ops_fetch_origin_fails_without_a_remote() {
        let tmp = TempDir::new().unwrap();
        git_init(tmp.path());

        let ops = ShellGitOps::new();
        let err = ops.fetch_origin(tmp.path()).unwrap_err();
        assert!(matches!(err, GitError::Command { .. }));
    }

    #[test]
    fn shell_git_ops_remove_worktree_removes_it() {
        let tmp = TempDir::new().unwrap();
        git_init(tmp.path());
        let wt_path = tmp.path().join("wt");

        let ops = ShellGitOps::new();
        ops.provision_worktree(tmp.path(), &wt_path, "removable-branch", None)
            .unwrap();
        assert!(wt_path.exists());

        ops.remove_worktree(tmp.path(), &wt_path).unwrap();
        assert!(!wt_path.exists());
    }

    #[test]
    fn shell_git_ops_remove_worktree_fails_on_dirty_worktree() {
        let tmp = TempDir::new().unwrap();
        git_init(tmp.path());
        let wt_path = tmp.path().join("wt");

        let ops = ShellGitOps::new();
        ops.provision_worktree(tmp.path(), &wt_path, "dirty-branch", None)
            .unwrap();
        std::fs::write(wt_path.join("dirty.txt"), "uncommitted\n").unwrap();

        let err = ops.remove_worktree(tmp.path(), &wt_path).unwrap_err();
        assert!(matches!(err, GitError::Command { .. }));
    }

    #[test]
    fn shell_git_ops_config_get_returns_none_for_unset_key() {
        let tmp = TempDir::new().unwrap();
        git_init(tmp.path());

        let ops = ShellGitOps::new();
        assert_eq!(ops.config_get(tmp.path(), "j.branchOwner").unwrap(), None);
    }

    #[test]
    fn shell_git_ops_config_get_returns_set_value() {
        let tmp = TempDir::new().unwrap();
        git_init(tmp.path());
        StdCommand::new("git")
            .arg("-C")
            .arg(tmp.path())
            .args(["config", "j.branchOwner", "jowi-dev"])
            .status()
            .unwrap();

        let ops = ShellGitOps::new();
        assert_eq!(
            ops.config_get(tmp.path(), "j.branchOwner").unwrap(),
            Some("jowi-dev".to_string())
        );
    }

    #[test]
    fn shell_git_ops_switch_new_branch_does_not_set_upstream() {
        let tmp = TempDir::new().unwrap();
        git_init(tmp.path());

        let ops = ShellGitOps::new();
        ops.switch_new_branch(tmp.path(), "run-branch", "main")
            .unwrap();

        let output = StdCommand::new("git")
            .arg("-C")
            .arg(tmp.path())
            .args(["rev-parse", "--abbrev-ref", "run-branch@{upstream}"])
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "expected no upstream to be configured after switch_new_branch"
        );
    }
}
