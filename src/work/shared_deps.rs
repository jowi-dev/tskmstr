//! Sharing dependency directories between a repo's main checkout and its
//! worktrees (GitHub issue #255).
//!
//! Every worktree normally builds its own copy of its dependencies (`deps`
//! for Elixir, `node_modules` for npm), even though most lane runs never
//! change them. With `[work] shared_deps` configured, a new worktree gets a
//! **symlink** to the main checkout's directory instead, as long as the
//! worktree's lock file matches the main checkout's byte for byte.
//!
//! A symlinked directory is shared: a dependency command run in one
//! worktree would rewrite it under every other worktree and the main
//! checkout. So before anything changes dependencies, the worktree must
//! **unshare**: replace the link with a private copy. [`unshare`] does that,
//! either unconditionally ([`UnshareMode::Always`], what
//! `tm work deps unshare` runs before an agent edits a lock file) or only
//! where the lock files already differ ([`UnshareMode::WhenLockDiffers`],
//! what `tm work run` checks at lane start). The copy is an APFS
//! copy-on-write clone on macOS (`cp -c`), so it costs no disk until a file
//! in it is rewritten.
//!
//! Removing a worktree never touches the main checkout's directory: `git
//! worktree remove` deletes the symlink itself, not what it points to.

use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use crate::config::SharedDep;
use crate::work::git::{GitError, GitOps};

/// Which linked paths [`unshare`] replaces with a private copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnshareMode {
    /// Every configured path that is currently a symlink.
    Always,
    /// Only linked paths whose lock file in the worktree is missing or
    /// differs from the main checkout's.
    WhenLockDiffers,
}

/// Whether `path` is a plain relative path that stays inside the directory
/// it is joined to: not empty, not absolute, and with no `..` components.
/// Config validation rejects anything else, because [`unshare`] deletes and
/// replaces what sits at the joined path.
pub fn is_contained_relative_path(path: &str) -> bool {
    let p = Path::new(path);
    !path.is_empty()
        && p.components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
        && p.components().any(|c| matches!(c, Component::Normal(_)))
}

/// Provision a worktree with [`GitOps::provision_worktree`], then link the
/// configured shared dependency paths into it. Returns the lines to print:
/// "Linked .env.local from main repo" when that link was made, the linked
/// shared paths, or a warning when linking failed. A failed link is not an
/// error: the worktree just builds its own dependencies, as it would with
/// no `shared_deps` configured.
pub fn provision_worktree(
    git: &dyn GitOps,
    repo_dir: &Path,
    wt_path: &Path,
    branch: &str,
    from_base: Option<&str>,
    shared: &[SharedDep],
) -> Result<Vec<String>, GitError> {
    let mut messages = Vec::new();
    if git.provision_worktree(repo_dir, wt_path, branch, from_base)? {
        messages.push("Linked .env.local from main repo".to_string());
    }
    match link_shared_deps(repo_dir, wt_path, shared) {
        Ok(linked) if linked.is_empty() => {}
        Ok(linked) => messages.push(format!(
            "Linked shared deps from main repo: {}",
            linked.join(", ")
        )),
        Err(err) => messages.push(format!("warning: could not link shared deps: {err}")),
    }
    Ok(messages)
}

/// Symlink each configured path in `wt_path` to the same path in
/// `repo_dir`, returning the paths that were linked.
///
/// A path is linked only when the main checkout has it as a directory, both
/// checkouts have the lock file with identical contents, and the worktree
/// has nothing at that path yet. Each linked path is also added to the
/// repository's `info/exclude` (see [`exclude_linked_path`]), so the link
/// does not show up as an untracked file and make the worktree look dirty.
pub fn link_shared_deps(
    repo_dir: &Path,
    wt_path: &Path,
    entries: &[SharedDep],
) -> io::Result<Vec<String>> {
    let mut linked = Vec::new();
    for entry in entries {
        let src = repo_dir.join(&entry.path);
        let dest = wt_path.join(&entry.path);
        if !src.is_dir() || dest.symlink_metadata().is_ok() {
            continue;
        }
        if !lock_files_match(repo_dir, wt_path, &entry.lock_file) {
            continue;
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::os::unix::fs::symlink(&src, &dest)?;
        exclude_linked_path(repo_dir, &entry.path)?;
        linked.push(entry.path.clone());
    }
    Ok(linked)
}

/// Replace linked paths in `wt_path` with private copies of what they point
/// to, returning the paths that were unshared. Paths that are not symlinks
/// (never linked, or already unshared) are left alone.
///
/// The copy lands next to the link under a temporary name first and is
/// renamed over the link only once complete, so a failed copy leaves the
/// link in place rather than a half-copied directory.
pub fn unshare(
    repo_dir: &Path,
    wt_path: &Path,
    entries: &[SharedDep],
    mode: UnshareMode,
) -> io::Result<Vec<String>> {
    let mut unshared = Vec::new();
    for entry in entries {
        let dest = wt_path.join(&entry.path);
        let is_link = dest
            .symlink_metadata()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        if !is_link {
            continue;
        }
        if mode == UnshareMode::WhenLockDiffers
            && lock_files_match(repo_dir, wt_path, &entry.lock_file)
        {
            continue;
        }
        let target = std::fs::read_link(&dest)?;
        let target = if target.is_absolute() {
            target
        } else {
            dest.parent().unwrap_or(wt_path).join(target)
        };
        let tmp = temp_sibling(&dest);
        if tmp.symlink_metadata().is_ok() {
            std::fs::remove_dir_all(&tmp)?;
        }
        copy_dir(&target, &tmp)?;
        std::fs::remove_file(&dest)?;
        std::fs::rename(&tmp, &dest)?;
        unshared.push(entry.path.clone());
    }
    Ok(unshared)
}

/// Whether `lock_file` exists in both checkouts with identical contents.
fn lock_files_match(repo_dir: &Path, wt_path: &Path, lock_file: &str) -> bool {
    match (
        std::fs::read(repo_dir.join(lock_file)),
        std::fs::read(wt_path.join(lock_file)),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Add `/<path>` to `<repo_dir>/.git/info/exclude` if it isn't there yet.
///
/// Needed because a typical ignore rule for a dependency directory has a
/// trailing slash (`/deps/`), which git matches only against directories;
/// a symlink is not a directory to git, so without this the link shows up
/// as untracked and `tm work run` refuses the worktree as dirty. Worktrees
/// share the main repository's `info/exclude`, so one line covers them all.
/// Does nothing when `repo_dir/.git` is not a directory.
fn exclude_linked_path(repo_dir: &Path, path: &str) -> io::Result<()> {
    let git_dir = repo_dir.join(".git");
    if !git_dir.is_dir() {
        return Ok(());
    }
    let info = git_dir.join("info");
    std::fs::create_dir_all(&info)?;
    let exclude = info.join("exclude");
    let line = format!("/{}", path.trim_end_matches('/'));
    let existing = std::fs::read_to_string(&exclude).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == line) {
        return Ok(());
    }
    let mut contents = existing;
    if !contents.is_empty() && !contents.ends_with('\n') {
        contents.push('\n');
    }
    contents.push_str(&line);
    contents.push('\n');
    std::fs::write(&exclude, contents)
}

/// `<dest>.tm-unshare`, the temporary name [`unshare`] copies into.
fn temp_sibling(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".tm-unshare");
    dest.with_file_name(name)
}

/// Recursively copy `src` to `dest` (which must not exist), preserving
/// symlinks and timestamps. On macOS this tries a copy-on-write clone
/// (`cp -c`) first and falls back to a plain copy when the filesystem
/// can't clone; elsewhere it asks `cp` for a reflink where supported.
fn copy_dir(src: &Path, dest: &Path) -> io::Result<()> {
    let attempts: &[&[&str]] = if cfg!(target_os = "macos") {
        &[&["-c", "-pR"], &["-pR"]]
    } else {
        &[&["--reflink=auto", "-pR"]]
    };
    let mut last_err = String::new();
    for flags in attempts {
        let output = Command::new("cp")
            .args(*flags)
            .arg(src)
            .arg(dest)
            .output()?;
        if output.status.success() {
            return Ok(());
        }
        last_err = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if dest.symlink_metadata().is_ok() {
            std::fs::remove_dir_all(dest)?;
        }
    }
    Err(io::Error::other(format!(
        "copying {} to {} failed: {last_err}",
        src.display(),
        dest.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn mix_deps() -> Vec<SharedDep> {
        vec![SharedDep {
            path: "deps".to_string(),
            lock_file: "mix.lock".to_string(),
        }]
    }

    /// A main checkout with `deps/pkg/lib.ex` and `mix.lock`, plus an empty
    /// worktree directory carrying `wt_lock` as its `mix.lock`.
    fn setup(wt_lock: Option<&str>) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root = tempdir().unwrap();
        let repo = root.path().join("repo");
        let wt = root.path().join("wt");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(repo.join("deps/pkg")).unwrap();
        fs::write(repo.join("deps/pkg/lib.ex"), "main").unwrap();
        fs::write(repo.join("mix.lock"), "lock-v1").unwrap();
        fs::create_dir_all(&wt).unwrap();
        if let Some(lock) = wt_lock {
            fs::write(wt.join("mix.lock"), lock).unwrap();
        }
        (root, repo, wt)
    }

    fn is_symlink(p: &Path) -> bool {
        p.symlink_metadata()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
    }

    #[test]
    fn links_configured_path_when_lock_files_match() {
        let (_root, repo, wt) = setup(Some("lock-v1"));

        let linked = link_shared_deps(&repo, &wt, &mix_deps()).unwrap();

        assert_eq!(linked, vec!["deps".to_string()]);
        assert!(is_symlink(&wt.join("deps")));
        assert_eq!(fs::read_link(wt.join("deps")).unwrap(), repo.join("deps"));
    }

    #[test]
    fn does_not_link_when_lock_files_differ() {
        let (_root, repo, wt) = setup(Some("lock-v2"));

        let linked = link_shared_deps(&repo, &wt, &mix_deps()).unwrap();

        assert!(linked.is_empty());
        assert!(wt.join("deps").symlink_metadata().is_err());
    }

    #[test]
    fn does_not_link_when_worktree_lock_file_is_missing() {
        let (_root, repo, wt) = setup(None);

        assert!(
            link_shared_deps(&repo, &wt, &mix_deps())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn does_not_link_when_main_checkout_has_no_directory() {
        let (_root, repo, wt) = setup(Some("lock-v1"));
        fs::remove_dir_all(repo.join("deps")).unwrap();

        assert!(
            link_shared_deps(&repo, &wt, &mix_deps())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn does_not_replace_an_existing_worktree_directory() {
        let (_root, repo, wt) = setup(Some("lock-v1"));
        fs::create_dir_all(wt.join("deps")).unwrap();

        assert!(
            link_shared_deps(&repo, &wt, &mix_deps())
                .unwrap()
                .is_empty()
        );
        assert!(!is_symlink(&wt.join("deps")));
    }

    #[test]
    fn no_entries_links_nothing() {
        let (_root, repo, wt) = setup(Some("lock-v1"));

        assert!(link_shared_deps(&repo, &wt, &[]).unwrap().is_empty());
        assert!(wt.join("deps").symlink_metadata().is_err());
    }

    #[test]
    fn linking_adds_path_to_info_exclude_once() {
        let (_root, repo, wt) = setup(Some("lock-v1"));
        let wt2 = wt.with_file_name("wt2");
        fs::create_dir_all(&wt2).unwrap();
        fs::write(wt2.join("mix.lock"), "lock-v1").unwrap();

        link_shared_deps(&repo, &wt, &mix_deps()).unwrap();
        link_shared_deps(&repo, &wt2, &mix_deps()).unwrap();

        let exclude = fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.lines().filter(|l| *l == "/deps").count(), 1);
    }

    #[test]
    fn unshare_when_lock_differs_copies_and_leaves_main_untouched() {
        let (_root, repo, wt) = setup(Some("lock-v1"));
        link_shared_deps(&repo, &wt, &mix_deps()).unwrap();
        fs::write(wt.join("mix.lock"), "lock-v2").unwrap();

        let unshared = unshare(&repo, &wt, &mix_deps(), UnshareMode::WhenLockDiffers).unwrap();

        assert_eq!(unshared, vec!["deps".to_string()]);
        assert!(!is_symlink(&wt.join("deps")));
        assert_eq!(
            fs::read_to_string(wt.join("deps/pkg/lib.ex")).unwrap(),
            "main"
        );
        // A dependency command now writes only the private copy.
        fs::write(wt.join("deps/pkg/lib.ex"), "changed").unwrap();
        assert_eq!(
            fs::read_to_string(repo.join("deps/pkg/lib.ex")).unwrap(),
            "main"
        );
        assert!(wt.join("deps.tm-unshare").symlink_metadata().is_err());
    }

    #[test]
    fn unshare_when_lock_differs_keeps_link_while_locks_match() {
        let (_root, repo, wt) = setup(Some("lock-v1"));
        link_shared_deps(&repo, &wt, &mix_deps()).unwrap();

        let unshared = unshare(&repo, &wt, &mix_deps(), UnshareMode::WhenLockDiffers).unwrap();

        assert!(unshared.is_empty());
        assert!(is_symlink(&wt.join("deps")));
    }

    #[test]
    fn unshare_always_copies_even_with_matching_locks() {
        let (_root, repo, wt) = setup(Some("lock-v1"));
        link_shared_deps(&repo, &wt, &mix_deps()).unwrap();

        let unshared = unshare(&repo, &wt, &mix_deps(), UnshareMode::Always).unwrap();

        assert_eq!(unshared, vec!["deps".to_string()]);
        assert!(!is_symlink(&wt.join("deps")));
        assert!(wt.join("deps/pkg/lib.ex").is_file());
    }

    #[test]
    fn unshare_leaves_real_directories_alone() {
        let (_root, repo, wt) = setup(Some("lock-v2"));
        fs::create_dir_all(wt.join("deps")).unwrap();
        fs::write(wt.join("deps/own.ex"), "own").unwrap();

        let unshared = unshare(&repo, &wt, &mix_deps(), UnshareMode::Always).unwrap();

        assert!(unshared.is_empty());
        assert_eq!(fs::read_to_string(wt.join("deps/own.ex")).unwrap(), "own");
    }

    // --- against a real git repository and worktree ---

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.email=t@example.com", "-c", "user.name=T"])
            .args(args)
            .status()
            .expect("git should run");
        assert!(status.success(), "git {args:?} failed");
    }

    /// A git repo whose `.gitignore` ignores `/deps/` the way Phoenix
    /// projects do (directory-only), with `deps/` populated, plus a linked
    /// worktree provisioned by [`ShellGitOps`].
    fn git_setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
        use crate::work::git::{GitOps, ShellGitOps};
        let root = tempdir().unwrap();
        let repo = root.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        fs::write(repo.join(".gitignore"), "/deps/\n").unwrap();
        fs::write(repo.join("mix.lock"), "lock-v1").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        fs::create_dir_all(repo.join("deps/pkg")).unwrap();
        fs::write(repo.join("deps/pkg/lib.ex"), "main").unwrap();
        let wt = root.path().join("wt");
        ShellGitOps::new()
            .provision_worktree(&repo, &wt, "lane", None)
            .unwrap();
        (root, repo, wt)
    }

    #[test]
    fn linked_worktree_is_clean_despite_directory_only_ignore_rule() {
        use crate::work::git::{GitOps, ShellGitOps};
        let (_root, repo, wt) = git_setup();

        let linked = link_shared_deps(&repo, &wt, &mix_deps()).unwrap();

        assert_eq!(linked, vec!["deps".to_string()]);
        assert!(ShellGitOps::new().status_is_clean(&wt).unwrap());
    }

    #[test]
    fn removing_a_linked_worktree_keeps_the_main_checkout_directory() {
        use crate::work::git::{GitOps, ShellGitOps};
        let (_root, repo, wt) = git_setup();
        link_shared_deps(&repo, &wt, &mix_deps()).unwrap();

        ShellGitOps::new().remove_worktree(&repo, &wt).unwrap();

        assert!(!wt.exists());
        assert_eq!(
            fs::read_to_string(repo.join("deps/pkg/lib.ex")).unwrap(),
            "main"
        );
    }

    #[test]
    fn worktree_that_edits_its_lock_file_gets_a_private_copy() {
        let (_root, repo, wt) = git_setup();
        link_shared_deps(&repo, &wt, &mix_deps()).unwrap();
        fs::write(wt.join("mix.lock"), "lock-v2").unwrap();

        let unshared = unshare(&repo, &wt, &mix_deps(), UnshareMode::WhenLockDiffers).unwrap();

        assert_eq!(unshared, vec!["deps".to_string()]);
        assert!(!is_symlink(&wt.join("deps")));
        fs::write(wt.join("deps/pkg/lib.ex"), "upgraded").unwrap();
        assert_eq!(
            fs::read_to_string(repo.join("deps/pkg/lib.ex")).unwrap(),
            "main"
        );
    }

    #[test]
    fn provision_links_shared_deps_and_reports_it() {
        use crate::work::git::ShellGitOps;
        let root = tempdir().unwrap();
        let repo = root.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        fs::write(repo.join(".gitignore"), "/deps/\n").unwrap();
        fs::write(repo.join("mix.lock"), "lock-v1").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        fs::create_dir_all(repo.join("deps/pkg")).unwrap();
        let wt = root.path().join("wt");

        let messages =
            provision_worktree(&ShellGitOps::new(), &repo, &wt, "lane", None, &mix_deps()).unwrap();

        assert!(is_symlink(&wt.join("deps")));
        assert_eq!(
            messages,
            vec!["Linked shared deps from main repo: deps".to_string()]
        );
    }

    #[test]
    fn provision_without_shared_deps_only_provisions() {
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
