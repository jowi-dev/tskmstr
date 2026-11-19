//! Seeding a new worktree with copies of the main checkout's dependency
//! and build directories (GitHub issues #255 and #259).
//!
//! Every worktree normally downloads and builds its own dependencies, even
//! though most lane runs never change them. With `[work] copy_dirs`
//! configured (e.g. `["deps", "_build"]`), provisioning a worktree copies each listed
//! directory from the main checkout. On macOS the copy is an APFS
//! copy-on-write clone (`/bin/cp -c`): the clone shares the original's disk
//! blocks and uses new space only for files later rewritten, so an
//! unchanged `deps` costs almost nothing. Elsewhere `cp` uses a reflink
//! where the filesystem supports one, and a full copy otherwise.
//!
//! The copy is the worktree's own directory, not a link: a dependency
//! install in the worktree changes only its copy, never the main
//! checkout's. That holds whether or not the worktree's lock file matches,
//! so no lock file check is needed; a mismatched copy is just a head start
//! for the install. Build output (`_build`, `target`) is safe to copy for
//! the same reason: the build tool rebuilds whatever is stale.

use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use crate::work::git::{GitError, GitOps};

/// Whether `path` is a plain relative path that stays inside the directory
/// it is joined to: not empty, not absolute, and with no `..` components.
/// Config validation rejects anything else, so a copy can never be written
/// outside the worktree.
pub fn is_contained_relative_path(path: &str) -> bool {
    let p = Path::new(path);
    !path.is_empty()
        && p.components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
        && p.components().any(|c| matches!(c, Component::Normal(_)))
}

/// Provision a worktree with [`GitOps::provision_worktree`], then copy each
/// of `dirs` from `repo_dir` into it. Returns the lines to print: "Linked
/// .env.local from main repo" when that link was made, then
/// [`CopyReport::messages`]. A failed copy is not an error: the worktree
/// just builds that directory itself, as it would with no `copy_dirs`
/// configured.
pub fn provision_worktree(
    git: &dyn GitOps,
    repo_dir: &Path,
    wt_path: &Path,
    branch: &str,
    from_base: Option<&str>,
    dirs: &[String],
) -> Result<Vec<String>, GitError> {
    let mut messages = Vec::new();
    if git.provision_worktree(repo_dir, wt_path, branch, from_base)? {
        messages.push("Linked .env.local from main repo".to_string());
    }
    messages.extend(copy_dirs(repo_dir, wt_path, dirs).messages());
    Ok(messages)
}

/// How a directory ended up in the worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyMethod {
    /// A copy-on-write clone that shares the original's disk blocks.
    Clone,
    /// A plain copy that takes the directory's full size on disk, made
    /// when the filesystem can't clone.
    FullCopy,
}

/// What [`copy_dirs`] did with each configured directory. Directories it
/// skipped (missing from the main checkout, or already in the worktree)
/// appear in none of the lists.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CopyReport {
    /// Directories copied as a copy-on-write clone.
    pub cloned: Vec<String>,
    /// Directories copied in full because the clone failed.
    pub full_copies: Vec<String>,
    /// One warning per directory that could not be copied at all.
    pub warnings: Vec<String>,
}

impl CopyReport {
    /// The lines to print: one naming the cloned directories, one naming
    /// the fully copied ones, then the warnings. A list with nothing in it
    /// prints nothing.
    pub fn messages(&self) -> Vec<String> {
        let mut messages = Vec::new();
        if !self.cloned.is_empty() {
            messages.push(format!("Cloned from main repo: {}", self.cloned.join(", ")));
        }
        if !self.full_copies.is_empty() {
            messages.push(format!(
                "Copied from main repo (full copy, this filesystem can't clone): {}",
                self.full_copies.join(", ")
            ));
        }
        messages.extend(self.warnings.iter().cloned());
        messages
    }
}

/// Copy each of `dirs` from `repo_dir` into `wt_path` and report how each
/// one was copied. A directory is skipped when the main checkout doesn't
/// have it or the worktree already has something at that path.
pub fn copy_dirs(repo_dir: &Path, wt_path: &Path, dirs: &[String]) -> CopyReport {
    copy_dirs_with(repo_dir, wt_path, dirs, &copy_attempts())
}

/// [`copy_dirs`] with the `cp` invocations to try passed in, so tests can
/// make the clone fail.
fn copy_dirs_with(
    repo_dir: &Path,
    wt_path: &Path,
    dirs: &[String],
    attempts: &[CopyAttempt],
) -> CopyReport {
    let mut report = CopyReport::default();
    for dir in dirs {
        let src = repo_dir.join(dir);
        let dest = wt_path.join(dir);
        if !src.is_dir() || dest.symlink_metadata().is_ok() {
            continue;
        }
        match copy_dir(&src, &dest, attempts) {
            Ok(CopyMethod::Clone) => report.cloned.push(dir.clone()),
            Ok(CopyMethod::FullCopy) => report.full_copies.push(dir.clone()),
            Err(err) => report
                .warnings
                .push(format!("warning: could not copy {dir}: {err}")),
        }
    }
    report
}

/// `<dest>.tm-copy`, the temporary name [`copy_dir`] copies into before
/// renaming, so a failed copy never leaves a half-copied directory at
/// `dest`.
fn temp_sibling(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".tm-copy");
    dest.with_file_name(name)
}

/// One `cp` invocation [`copy_dir`] may try, and the kind of copy it makes
/// when it succeeds.
#[derive(Debug, Clone, Copy)]
struct CopyAttempt {
    program: &'static str,
    flags: [&'static str; 2],
    method: CopyMethod,
}

/// The `cp` invocations [`copy_dir`] tries in order: a copy-on-write clone,
/// then a plain copy for a filesystem that can't clone.
///
/// On macOS the clone is the system `/bin/cp -c`, called by absolute path
/// because the `cp` first on `PATH` may be GNU coreutils, which has no
/// `-c`. Elsewhere it is GNU `cp --reflink=always`, which fails instead of
/// quietly copying when the filesystem has no reflinks, so the report can
/// say which kind of copy was made.
fn copy_attempts() -> Vec<CopyAttempt> {
    if cfg!(target_os = "macos") {
        vec![
            CopyAttempt {
                program: "/bin/cp",
                flags: ["-c", "-pR"],
                method: CopyMethod::Clone,
            },
            CopyAttempt {
                program: "/bin/cp",
                flags: ["-p", "-R"],
                method: CopyMethod::FullCopy,
            },
        ]
    } else {
        vec![
            CopyAttempt {
                program: "cp",
                flags: ["--reflink=always", "-pR"],
                method: CopyMethod::Clone,
            },
            CopyAttempt {
                program: "cp",
                flags: ["-p", "-R"],
                method: CopyMethod::FullCopy,
            },
        ]
    }
}

/// Recursively copy `src` to `dest` (which must not exist), preserving
/// symlinks and timestamps, using the first of `attempts` that succeeds.
/// Returns that attempt's [`CopyMethod`], or the last attempt's error
/// output when every attempt fails.
fn copy_dir(src: &Path, dest: &Path, attempts: &[CopyAttempt]) -> io::Result<CopyMethod> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = temp_sibling(dest);
    let mut last_err = String::new();
    for attempt in attempts {
        if tmp.symlink_metadata().is_ok() {
            std::fs::remove_dir_all(&tmp)?;
        }
        let output = Command::new(attempt.program)
            .args(attempt.flags)
            .arg(src)
            .arg(&tmp)
            .output()?;
        if output.status.success() {
            std::fs::rename(&tmp, dest)?;
            return Ok(attempt.method);
        }
        last_err = String::from_utf8_lossy(&output.stderr).trim().to_string();
    }
    if tmp.symlink_metadata().is_ok() {
        std::fs::remove_dir_all(&tmp)?;
    }
    Err(io::Error::other(last_err))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn deps() -> Vec<String> {
        vec!["deps".to_string()]
    }

    /// A main checkout with `deps/pkg/lib.ex` and an empty worktree dir.
    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root = tempdir().unwrap();
        let repo = root.path().join("repo");
        let wt = root.path().join("wt");
        fs::create_dir_all(repo.join("deps/pkg")).unwrap();
        fs::write(repo.join("deps/pkg/lib.ex"), "main").unwrap();
        fs::create_dir_all(&wt).unwrap();
        (root, repo, wt)
    }

    #[test]
    fn copies_configured_directory_as_a_real_directory() {
        let (_root, repo, wt) = setup();

        let report = copy_dirs(&repo, &wt, &deps());

        assert_eq!(report.cloned.len() + report.full_copies.len(), 1);
        assert!(report.warnings.is_empty());
        let meta = wt.join("deps").symlink_metadata().unwrap();
        assert!(meta.is_dir() && !meta.file_type().is_symlink());
        assert_eq!(
            fs::read_to_string(wt.join("deps/pkg/lib.ex")).unwrap(),
            "main"
        );
    }

    #[test]
    fn writes_to_the_copy_leave_the_main_checkout_untouched() {
        let (_root, repo, wt) = setup();
        copy_dirs(&repo, &wt, &deps());

        fs::write(wt.join("deps/pkg/lib.ex"), "upgraded").unwrap();
        fs::write(wt.join("deps/pkg/new.ex"), "new").unwrap();

        assert_eq!(
            fs::read_to_string(repo.join("deps/pkg/lib.ex")).unwrap(),
            "main"
        );
        assert!(!repo.join("deps/pkg/new.ex").exists());
    }

    #[test]
    fn copies_a_nested_directory() {
        let (_root, repo, wt) = setup();
        fs::create_dir_all(repo.join("assets/node_modules/x")).unwrap();
        let dirs = vec!["assets/node_modules".to_string()];

        let report = copy_dirs(&repo, &wt, &dirs);

        assert!(report.warnings.is_empty());
        assert!(wt.join("assets/node_modules/x").is_dir());
    }

    #[test]
    fn skips_a_directory_the_main_checkout_lacks() {
        let (_root, repo, wt) = setup();
        fs::remove_dir_all(repo.join("deps")).unwrap();

        let report = copy_dirs(&repo, &wt, &deps());

        assert_eq!(report, CopyReport::default());
        assert!(!wt.join("deps").exists());
    }

    #[test]
    fn leaves_an_existing_worktree_directory_alone() {
        let (_root, repo, wt) = setup();
        fs::create_dir_all(wt.join("deps")).unwrap();
        fs::write(wt.join("deps/own.ex"), "own").unwrap();

        let report = copy_dirs(&repo, &wt, &deps());

        assert_eq!(report, CopyReport::default());
        assert!(!wt.join("deps/pkg").exists());
        assert_eq!(fs::read_to_string(wt.join("deps/own.ex")).unwrap(), "own");
    }

    #[test]
    fn no_dirs_copies_nothing() {
        let (_root, repo, wt) = setup();

        let report = copy_dirs(&repo, &wt, &[]);

        assert_eq!(report, CopyReport::default());
        assert!(!wt.join("deps").exists());
    }

    #[test]
    fn provision_copies_dirs_and_reports_them() {
        use crate::work::git::ShellGitOps;
        let root = tempdir().unwrap();
        let repo = root.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        fs::write(repo.join(".gitignore"), "/deps/\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        fs::create_dir_all(repo.join("deps/pkg")).unwrap();
        let wt = root.path().join("wt");

        let messages =
            provision_worktree(&ShellGitOps::new(), &repo, &wt, "lane", None, &deps()).unwrap();

        assert!(wt.join("deps/pkg").is_dir());
        assert_eq!(messages.len(), 1);
        assert!(messages[0].ends_with(": deps"), "{messages:?}");
        // The copy is ignored like the original, so the worktree is clean.
        assert!(ShellGitOps::new().status_is_clean(&wt).unwrap());
    }

    #[test]
    fn provision_without_dirs_only_provisions() {
        use crate::work::git::FakeGitOps;
        let fake = FakeGitOps::new();

        let messages = provision_worktree(
            &fake,
            Path::new("/repo"),
            Path::new("/wt"),
            "lane",
            Some("main"),
            &[],
        )
        .unwrap();

        assert!(messages.is_empty());
        assert_eq!(fake.provision_worktree_calls().len(), 1);
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.email=t@example.com", "-c", "user.name=T"])
            .args(args)
            .status()
            .expect("git should run");
        assert!(status.success(), "git {args:?} failed");
    }

    /// The `cp` first on `PATH` may be GNU coreutils (from nix or
    /// Homebrew), which has no `-c` and would silently fall back to a full
    /// copy, so macOS must call the system `cp` by absolute path.
    #[test]
    #[cfg(target_os = "macos")]
    fn macos_clones_with_the_system_cp_first() {
        let attempts = copy_attempts();
        assert_eq!(attempts[0].program, "/bin/cp");
        assert_eq!(attempts[0].flags, ["-c", "-pR"]);
        assert_eq!(attempts[0].method, CopyMethod::Clone);
    }

    /// Attempts that clone with a `cp` that always fails, then fall back
    /// to a plain copy, as on a filesystem that can't clone.
    fn clone_fails_attempts() -> Vec<CopyAttempt> {
        vec![
            CopyAttempt {
                program: "false",
                flags: ["-c", "-pR"],
                method: CopyMethod::Clone,
            },
            CopyAttempt {
                program: "cp",
                flags: ["-p", "-R"],
                method: CopyMethod::FullCopy,
            },
        ]
    }

    #[test]
    fn a_successful_first_attempt_is_reported_as_a_clone() {
        let (_root, repo, wt) = setup();
        let attempts = [CopyAttempt {
            program: "cp",
            flags: ["-p", "-R"],
            method: CopyMethod::Clone,
        }];

        let report = copy_dirs_with(&repo, &wt, &deps(), &attempts);

        assert_eq!(report.cloned, deps());
        assert!(report.full_copies.is_empty() && report.warnings.is_empty());
    }

    #[test]
    fn a_failed_clone_falls_back_to_a_full_copy_and_reports_it() {
        let (_root, repo, wt) = setup();

        let report = copy_dirs_with(&repo, &wt, &deps(), &clone_fails_attempts());

        assert!(report.cloned.is_empty());
        assert_eq!(report.full_copies, deps());
        assert!(report.warnings.is_empty());
        assert_eq!(
            fs::read_to_string(wt.join("deps/pkg/lib.ex")).unwrap(),
            "main"
        );
        assert!(!wt.join("deps.tm-copy").exists());
    }

    #[test]
    fn report_messages_say_which_dirs_were_cloned_and_which_fully_copied() {
        let report = CopyReport {
            cloned: vec!["deps".to_string(), "_build".to_string()],
            full_copies: vec!["target".to_string()],
            warnings: vec!["warning: could not copy x: boom".to_string()],
        };

        assert_eq!(
            report.messages(),
            vec![
                "Cloned from main repo: deps, _build".to_string(),
                "Copied from main repo (full copy, this filesystem can't clone): target"
                    .to_string(),
                "warning: could not copy x: boom".to_string(),
            ]
        );
    }

    #[test]
    fn an_empty_report_has_no_messages() {
        assert!(CopyReport::default().messages().is_empty());
    }

    #[test]
    fn contained_relative_path_rejects_escapes() {
        assert!(is_contained_relative_path("deps"));
        assert!(is_contained_relative_path("assets/node_modules"));
        assert!(!is_contained_relative_path(""));
        assert!(!is_contained_relative_path("/deps"));
        assert!(!is_contained_relative_path("../deps"));
        assert!(!is_contained_relative_path("a/../../deps"));
        assert!(!is_contained_relative_path("."));
    }
}
