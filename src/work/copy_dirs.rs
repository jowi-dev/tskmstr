//! Seeding a new worktree with copies of the main checkout's dependency
//! directories (GitHub issue #255).
//!
//! Every worktree normally downloads and builds its own dependencies, even
//! though most lane runs never change them. With `[work] copy_dirs`
//! configured (e.g. `["deps"]`), provisioning a worktree copies each listed
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
//! for the install.

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
/// .env.local from main repo" when that link was made, the copied
/// directories, and a warning per directory that failed to copy. A failed
/// copy is not an error: the worktree just builds that directory itself,
/// as it would with no `copy_dirs` configured.
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
    let (copied, warnings) = copy_dirs(repo_dir, wt_path, dirs);
    if !copied.is_empty() {
        messages.push(format!("Copied from main repo: {}", copied.join(", ")));
    }
    messages.extend(warnings);
    Ok(messages)
}

/// Copy each of `dirs` from `repo_dir` into `wt_path`, returning the
/// directories copied and the warnings for the ones that failed. A
/// directory is skipped when the main checkout doesn't have it or the
/// worktree already has something at that path.
pub fn copy_dirs(repo_dir: &Path, wt_path: &Path, dirs: &[String]) -> (Vec<String>, Vec<String>) {
    let mut copied = Vec::new();
    let mut warnings = Vec::new();
    for dir in dirs {
        let src = repo_dir.join(dir);
        let dest = wt_path.join(dir);
        if !src.is_dir() || dest.symlink_metadata().is_ok() {
            continue;
        }
        match copy_dir(&src, &dest) {
            Ok(()) => copied.push(dir.clone()),
            Err(err) => warnings.push(format!("warning: could not copy {dir}: {err}")),
        }
    }
    (copied, warnings)
}

/// `<dest>.tm-copy`, the temporary name [`copy_dir`] copies into before
/// renaming, so a failed copy never leaves a half-copied directory at
/// `dest`.
fn temp_sibling(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".tm-copy");
    dest.with_file_name(name)
}

/// The `cp` invocations [`copy_dir`] tries in order, as (program, flags).
///
/// On macOS the first is a copy-on-write clone with the system `/bin/cp`
/// (`-c`), called by absolute path because the `cp` first on `PATH` may be
/// GNU coreutils, which has no `-c`. The fallback is a plain copy, for a
/// filesystem that can't clone. Elsewhere one GNU `cp` call asks for a
/// reflink where the filesystem supports one and copies otherwise.
fn copy_attempts() -> Vec<(&'static str, [&'static str; 2])> {
    if cfg!(target_os = "macos") {
        vec![("/bin/cp", ["-c", "-pR"]), ("/bin/cp", ["-p", "-R"])]
    } else {
        vec![("cp", ["--reflink=auto", "-pR"])]
    }
}

/// Recursively copy `src` to `dest` (which must not exist), preserving
/// symlinks and timestamps, using the first of [`copy_attempts`] that
/// succeeds.
fn copy_dir(src: &Path, dest: &Path) -> io::Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = temp_sibling(dest);
    let mut last_err = String::new();
    for (program, flags) in copy_attempts() {
        if tmp.symlink_metadata().is_ok() {
            std::fs::remove_dir_all(&tmp)?;
        }
        let output = Command::new(program)
            .args(flags)
            .arg(src)
            .arg(&tmp)
            .output()?;
        if output.status.success() {
            return std::fs::rename(&tmp, dest);
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

        let (copied, warnings) = copy_dirs(&repo, &wt, &deps());

        assert_eq!(copied, deps());
        assert!(warnings.is_empty());
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

        let (copied, _) = copy_dirs(&repo, &wt, &dirs);

        assert_eq!(copied, dirs);
        assert!(wt.join("assets/node_modules/x").is_dir());
    }

    #[test]
    fn skips_a_directory_the_main_checkout_lacks() {
        let (_root, repo, wt) = setup();
        fs::remove_dir_all(repo.join("deps")).unwrap();

        let (copied, warnings) = copy_dirs(&repo, &wt, &deps());

        assert!(copied.is_empty());
        assert!(warnings.is_empty());
        assert!(!wt.join("deps").exists());
    }

    #[test]
    fn leaves_an_existing_worktree_directory_alone() {
        let (_root, repo, wt) = setup();
        fs::create_dir_all(wt.join("deps")).unwrap();
        fs::write(wt.join("deps/own.ex"), "own").unwrap();

        let (copied, _) = copy_dirs(&repo, &wt, &deps());

        assert!(copied.is_empty());
        assert!(!wt.join("deps/pkg").exists());
        assert_eq!(fs::read_to_string(wt.join("deps/own.ex")).unwrap(), "own");
    }

    #[test]
    fn no_dirs_copies_nothing() {
        let (_root, repo, wt) = setup();

        let (copied, warnings) = copy_dirs(&repo, &wt, &[]);

        assert!(copied.is_empty() && warnings.is_empty());
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
        assert_eq!(messages, vec!["Copied from main repo: deps".to_string()]);
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
        assert_eq!(attempts[0].0, "/bin/cp");
        assert_eq!(attempts[0].1, ["-c", "-pR"]);
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
