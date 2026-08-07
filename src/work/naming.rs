//! Pure naming/path helpers for the lane runner, ported from devtools'
//! `~/devtools/work.ml`. Every function here takes all its inputs as
//! parameters — no environment reads, no clock reads (a formatted
//! timestamp is passed in, not computed) — so they need no fakes to test.

use std::path::{Path, PathBuf};

/// Build the path of a lane's worktree, mirroring `work.ml`'s
/// `worktree_path`:
///
/// ```ocaml
/// let worktree_path name =
///   Filename.concat (Filename.concat worktrees_root (repo_name ())) name
/// ```
///
/// `Filename.concat` never doubles the path separator: it inserts `/`
/// between its two arguments only when the first doesn't already end with
/// one. [`Path::join`] behaves the same way, so a trailing slash on
/// `worktree_root` (e.g. `~/Worktrees/`, a plausible config typo) still
/// yields a clean path rather than `~/Worktrees//repo/name`.
pub fn worktree_path(worktree_root: &str, repo_name: &str, name: &str) -> PathBuf {
    Path::new(worktree_root).join(repo_name).join(name)
}

/// Derive a tmux session name from a worktree/session directory, mirroring
/// `work.ml`'s `session_name_of_dir`:
///
/// ```ocaml
/// let session_name_of_dir dir =
///   let base = Filename.basename dir in
///   String.map (fun c -> if c = '.' then '-' else c) base
/// ```
///
/// Dots are the *only* character `work.ml` substitutes — there is no other
/// character mapping anywhere in the source (the similarly-named
/// `sanitize_branch_owner` strips characters outside `[A-Za-z0-9-_.]`, but
/// that's a different function for a different purpose, not applied here).
///
/// Edge case ported deliberately rather than "fixed": OCaml's
/// `Filename.basename ""` returns `"."`, which then survives the dot
/// substitution to become `"-"`. Rust's `Path::file_name` instead returns
/// `None` for both `""` and `"."`. To keep the observable output identical
/// to `work.ml` for this (unrealistic — callers always pass a real
/// worktree/cwd path) input, `None` is mapped to the same `"."` fallback
/// OCaml would have produced before substitution, which then becomes `"-"`
/// after it. Pinned by `session_name_from_dir_empty_dir_matches_ocaml_basename`.
pub fn session_name_from_dir(dir: &str) -> String {
    let base = Path::new(dir)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".to_string());
    base.replace('.', "-")
}

/// Format a broken-down local time as `work.ml`'s `timestamp` does:
///
/// ```ocaml
/// let timestamp () =
///   let tm = Unix.localtime (Unix.time ()) in
///   sprintf "%04d%02d%02d-%02d%02d%02d"
///     (tm.Unix.tm_year + 1900) (tm.Unix.tm_mon + 1) tm.Unix.tm_mday
///     tm.Unix.tm_hour tm.Unix.tm_min tm.Unix.tm_sec
/// ```
///
/// This function does not read the clock — the caller resolves "now" (in
/// whatever way step 3+ decides, e.g. via `libc::localtime`) and supplies
/// the already-broken-down components. `month` is 1-12 and `day` is the
/// day of month, matching the already-`+1`/already-1-indexed values
/// `work.ml` formats (i.e. pass `tm_mon + 1`, not raw `tm_mon`).
pub fn format_timestamp(year: i32, month: u32, day: u32, hour: u32, min: u32, sec: u32) -> String {
    format!("{year:04}{month:02}{day:02}-{hour:02}{min:02}{sec:02}")
}

/// Build a lane run's branch name, mirroring `work.ml`'s `run_lane`:
///
/// ```ocaml
/// let branch = sprintf "%s/%s-%s" owner wt_name (timestamp ())
/// ```
///
/// `timestamp` here is the already-formatted string from
/// [`format_timestamp`], not computed by this function.
pub fn branch_name(owner: &str, lane: &str, timestamp: &str) -> String {
    format!("{owner}/{lane}-{timestamp}")
}

/// Expand a leading `~` (or `~/...`) in `path` to `home`, mirroring the
/// convention `work.ml` gets for free from the OCaml stdlib not doing any
/// such expansion at all — `work.ml` hardcodes `worktrees_root` via
/// `Filename.concat (Sys.getenv "HOME") "Worktrees"` rather than storing a
/// `~`-prefixed string anywhere. `tm work`'s config stores
/// `worktree_root`/`repo` as plain strings that *may* contain a literal
/// `~` (e.g. `~/Worktrees` in a config file), and
/// [`crate::config::RawWorkConfig::worktree_root`]'s doc comment is explicit
/// that expansion is the caller's responsibility, not `config`'s — this is
/// that caller-side expansion, kept pure (no env read) so it can be tested
/// without touching `$HOME`.
///
/// Only a leading `~` is special-cased, matching shell tilde-expansion for
/// the current user (no `~otheruser` support, since none of `work.ml`'s
/// ported paths need it). A bare `~` expands to `home` itself; `~/rest`
/// expands to `home` joined with `rest`. Any other path (including one
/// merely containing a `~` elsewhere) is returned unchanged.
pub fn expand_tilde(path: &str, home: &Path) -> PathBuf {
    if path == "~" {
        home.to_path_buf()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_path_joins_root_repo_and_name() {
        let path = worktree_path("/Users/jowi/Worktrees", "axiom", "partner-integrations");
        assert_eq!(
            path,
            PathBuf::from("/Users/jowi/Worktrees/axiom/partner-integrations")
        );
    }

    #[test]
    fn worktree_path_handles_trailing_slash_root() {
        let path = worktree_path("/Users/jowi/Worktrees/", "axiom", "partner-integrations");
        assert_eq!(
            path,
            PathBuf::from("/Users/jowi/Worktrees/axiom/partner-integrations")
        );
    }

    #[test]
    fn session_name_from_dir_uses_last_component() {
        assert_eq!(
            session_name_from_dir("/Users/jowi/Worktrees/axiom/partner-integrations"),
            "partner-integrations"
        );
    }

    #[test]
    fn session_name_from_dir_replaces_dots_with_dashes() {
        assert_eq!(
            session_name_from_dir("/Users/jowi/Worktrees/axiom/TICKET-123.foo.bar"),
            "TICKET-123-foo-bar"
        );
    }

    #[test]
    fn session_name_from_dir_leaves_existing_dashes_alone() {
        // Names already containing the substitution target (a dash) are a
        // no-op under `String.map` — dashes aren't touched, only dots are.
        assert_eq!(
            session_name_from_dir("/Users/jowi/Worktrees/axiom/already-dashed"),
            "already-dashed"
        );
    }

    #[test]
    fn session_name_from_dir_handles_trailing_slash() {
        // OCaml's Filename.basename("/a/b/") = "b"; Path::file_name agrees.
        assert_eq!(
            session_name_from_dir("/Users/jowi/Worktrees/axiom/lane/"),
            "lane"
        );
    }

    #[test]
    fn session_name_from_dir_empty_dir_matches_ocaml_basename() {
        // Filename.basename "" = "." in OCaml, which then dot-substitutes to
        // "-". Ported verbatim even though no real caller should hit this.
        assert_eq!(session_name_from_dir(""), "-");
    }

    #[test]
    fn format_timestamp_matches_ocaml_format_string() {
        // sprintf "%04d%02d%02d-%02d%02d%02d" 2026 8 6 9 5 3
        assert_eq!(format_timestamp(2026, 8, 6, 9, 5, 3), "20260806-090503");
    }

    #[test]
    fn format_timestamp_pads_all_fields() {
        assert_eq!(format_timestamp(1, 1, 1, 1, 1, 1), "00010101-010101");
    }

    #[test]
    fn branch_name_matches_ocaml_format_string() {
        // sprintf "%s/%s-%s" owner wt_name (timestamp ())
        assert_eq!(
            branch_name("jowi-dev", "partner-integrations", "20260806-090503"),
            "jowi-dev/partner-integrations-20260806-090503"
        );
    }

    #[test]
    fn expand_tilde_expands_leading_tilde_slash() {
        assert_eq!(
            expand_tilde("~/Worktrees", Path::new("/Users/jowi")),
            PathBuf::from("/Users/jowi/Worktrees")
        );
    }

    #[test]
    fn expand_tilde_expands_bare_tilde() {
        assert_eq!(
            expand_tilde("~", Path::new("/Users/jowi")),
            PathBuf::from("/Users/jowi")
        );
    }

    #[test]
    fn expand_tilde_leaves_absolute_path_unchanged() {
        assert_eq!(
            expand_tilde("/Worktrees", Path::new("/Users/jowi")),
            PathBuf::from("/Worktrees")
        );
    }

    #[test]
    fn expand_tilde_leaves_non_leading_tilde_unchanged() {
        // Only a *leading* ~ is special-cased, matching shell semantics.
        assert_eq!(
            expand_tilde("/foo/~bar", Path::new("/Users/jowi")),
            PathBuf::from("/foo/~bar")
        );
    }

    #[test]
    fn branch_name_uses_ticket_scoped_lane_when_given() {
        // run_lane scopes wt_name to the lowercased ticket when one is
        // provided; branch_name itself is agnostic to that decision and just
        // formats whatever lane string it's handed.
        assert_eq!(
            branch_name("jowi-dev", "abc-123", "20260806-090503"),
            "jowi-dev/abc-123-20260806-090503"
        );
    }
}
