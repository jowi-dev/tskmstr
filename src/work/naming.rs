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

/// Belt-and-suspenders check for [`worktree_path`]'s empty-component
/// hazard: `PathBuf::join("")` is a no-op, so an empty `repo_name` or
/// `name` silently collapses the result one level short of a real
/// worktree path — landing on `worktree_root/repo_name` (or `worktree_root`
/// itself) rather than `worktree_root/repo_name/name`. That collapsed path
/// is exactly a project's per-repo *worktree directory*, so a caller that
/// doesn't catch this can end up running `git worktree add` directly into
/// it — which is how a real incident produced a worktree checkout sitting
/// at the root of `~/Worktrees/<repo>`, with `tm work restore` mistaking
/// its subdirectories (`lib/`, `test/`, ...) for worktrees of their own.
///
/// This is deliberately independent of validating that `repo_name`/`name`
/// are non-empty up front (see callers in `crate::work::run::prepare_run_lane`
/// and `crate::cli::work::worktree_path_for`): callers should reject empty
/// inputs before ever calling [`worktree_path`], and additionally re-check
/// the *result* against this function right before `git worktree add`, so a
/// bug in the emptiness check doesn't leave this hazard uncaught.
///
/// Returns `true` when `wt_path` is a real worktree path — its parent is
/// exactly `worktree_root/repo_name`, i.e. it sits one level below the
/// project's worktree directory, as a real worktree path always should.
pub fn worktree_path_has_expected_parent(
    worktree_root: &str,
    repo_name: &str,
    wt_path: &Path,
) -> bool {
    let expected_parent = Path::new(worktree_root).join(repo_name);
    wt_path.parent() == Some(expected_parent.as_path())
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

/// Build a lane run's branch name from a Jira-summary-derived slug rather
/// than a timestamp, mirroring [`branch_name`]'s shape but for the
/// human-readable naming scheme: `<owner>/<lane>-<slug>`. There is
/// deliberately no timestamp component here — the whole point of this
/// scheme is a readable branch name, and a timestamp would defeat that — so
/// unlike [`branch_name`], this alone does not guarantee uniqueness across
/// re-runs of the same lane/ticket. Callers are expected to run the
/// candidate this produces through [`resolve_branch_collision`] before
/// cutting the branch. See [`slugify_summary`] for how `slug` itself is
/// derived, and [`crate::work::run::prepare_run_lane`]'s step 6 for the
/// fallback that keeps using [`branch_name`] instead of this function
/// whenever no slug is available.
pub fn branch_name_with_slug(owner: &str, lane: &str, slug: &str) -> String {
    format!("{owner}/{lane}-{slug}")
}

/// English stopwords dropped from [`slugify_summary`]'s word list before
/// the first-3-words cut, so the slug carries content words
/// ("delete-bid-connector") rather than being padded with grammatical
/// glue ("the-delete-of"). Deliberately small and hand-picked rather than
/// an exhaustive list — good enough for typical Jira summaries, not a
/// general-purpose NLP stopword table.
const STOPWORDS: &[&str] = &[
    "the", "a", "an", "to", "of", "for", "in", "on", "at", "and", "or", "with", "from", "by", "is",
    "are", "be",
];

/// The maximum length, in bytes, of a slug returned by [`slugify_summary`].
/// A branch name is `<owner>/<lane>-<slug>` (or, with a collision suffix,
/// `<owner>/<lane>-<slug>-<n>`), and an unbounded slug from a long Jira
/// summary could make that unwieldy to type/read on a terminal — 40 is
/// generous for "2-3 words" while still capping the worst case.
const MAX_SLUG_LEN: usize = 40;

/// Derive a short, branch-name-safe slug from a Jira issue summary, for the
/// human-readable branch-naming scheme (see [`branch_name_with_slug`]).
///
/// The steps, in order:
/// 1. Every character that isn't ASCII alphanumeric is treated as a word
///    separator (mapped to a space) rather than simply dropped — this is
///    what turns a hyphenated summary like "Delete bid-connector" into the
///    three separate words `delete`, `bid`, `connector` rather than
///    collapsing `bid-connector` into one word. Case is folded to
///    lowercase in the same pass.
/// 2. The cleaned string is split on whitespace into words; each word is
///    therefore already restricted to `[a-z0-9]`.
/// 3. Words in [`STOPWORDS`] are dropped. If that leaves nothing (e.g. a
///    summary that's entirely stopwords, or one that's short enough that
///    stripping them empties it), the *un-filtered* word list is used
///    instead — a slug of stopwords beats no slug at all, and this
///    function's only "no slug" outcome is a summary with no words in it.
/// 4. The first three (possibly fewer) surviving words are joined with
///    `-`, then truncated to [`MAX_SLUG_LEN`] bytes (dropping a trailing
///    dangling `-` left by the cut, if any).
///
/// Returns `None` only when `summary` contains no alphanumeric characters
/// at all (empty string, pure punctuation/whitespace) — the case that maps
/// to `tm work run`'s existing timestamp-based fallback, per
/// [`crate::work::run::prepare_run_lane`]'s step 6.
pub fn slugify_summary(summary: &str) -> Option<String> {
    let cleaned: String = summary
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect();
    let words: Vec<&str> = cleaned.split_whitespace().collect();
    if words.is_empty() {
        return None;
    }

    let non_stopwords: Vec<&str> = words
        .iter()
        .copied()
        .filter(|w| !STOPWORDS.contains(w))
        .collect();
    let chosen: Vec<&str> = if non_stopwords.is_empty() {
        words
    } else {
        non_stopwords
    };

    let slug: String = chosen.into_iter().take(3).collect::<Vec<_>>().join("-");
    Some(truncate_slug(&slug))
}

/// Cap `slug` to [`MAX_SLUG_LEN`] bytes, trimming a trailing `-` the cut may
/// have left dangling. Every byte of `slug` is ASCII (guaranteed by
/// [`slugify_summary`]'s character mapping), so byte-slicing at
/// `MAX_SLUG_LEN` can never land mid-character.
fn truncate_slug(slug: &str) -> String {
    if slug.len() <= MAX_SLUG_LEN {
        return slug.to_string();
    }
    let mut truncated = slug[..MAX_SLUG_LEN].to_string();
    while truncated.ends_with('-') {
        truncated.pop();
    }
    truncated
}

/// Resolve a naming collision against an arbitrary existence check, for the
/// human-readable branch-naming scheme's uniqueness guarantee (see
/// [`branch_name_with_slug`]'s doc comment): unlike a timestamp suffix, a
/// slug can collide with a branch left over from an earlier run of the same
/// lane/ticket.
///
/// `exists` is injected rather than reading git directly, keeping this
/// function itself pure per the module doc comment; the real caller
/// ([`crate::work::run::prepare_run_lane`]) passes a closure that checks
/// both a local and an `origin`-remote-tracking ref. Starting from
/// `candidate`, this tries `candidate`, then `<candidate>-2`,
/// `<candidate>-3`, ... until `exists` reports one that's free, and returns
/// that one. `exists`'s `Err` propagates immediately (a broken existence
/// check should abort the run, not silently be treated as "free").
pub fn resolve_branch_collision<E>(
    candidate: &str,
    mut exists: impl FnMut(&str) -> Result<bool, E>,
) -> Result<String, E> {
    if !exists(candidate)? {
        return Ok(candidate.to_string());
    }
    let mut n = 2;
    loop {
        let next = format!("{candidate}-{n}");
        if !exists(&next)? {
            return Ok(next);
        }
        n += 1;
    }
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
    fn worktree_path_collapses_one_level_short_on_empty_name() {
        // Documents the hazard `worktree_path_has_expected_parent` guards
        // against: an empty `name` makes the trailing `.join("")` a no-op,
        // so the result lands on the project's worktree directory itself
        // rather than a real worktree path one level below it.
        let path = worktree_path("/Users/jowi/Worktrees", "axiom", "");
        assert_eq!(path, PathBuf::from("/Users/jowi/Worktrees/axiom"));
    }

    #[test]
    fn worktree_path_collapses_one_level_short_on_empty_repo_name() {
        let path = worktree_path("/Users/jowi/Worktrees", "", "partner-integrations");
        assert_eq!(
            path,
            PathBuf::from("/Users/jowi/Worktrees/partner-integrations")
        );
    }

    #[test]
    fn worktree_path_has_expected_parent_true_for_a_real_worktree_path() {
        let path = worktree_path("/Users/jowi/Worktrees", "axiom", "partner-integrations");
        assert!(worktree_path_has_expected_parent(
            "/Users/jowi/Worktrees",
            "axiom",
            &path
        ));
    }

    #[test]
    fn worktree_path_has_expected_parent_false_when_name_collapsed() {
        let path = worktree_path("/Users/jowi/Worktrees", "axiom", "");
        assert!(!worktree_path_has_expected_parent(
            "/Users/jowi/Worktrees",
            "axiom",
            &path
        ));
    }

    #[test]
    fn worktree_path_has_expected_parent_false_when_repo_name_collapsed() {
        let path = worktree_path("/Users/jowi/Worktrees", "", "partner-integrations");
        assert!(!worktree_path_has_expected_parent(
            "/Users/jowi/Worktrees",
            "axiom",
            &path
        ));
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
    fn branch_name_with_slug_has_no_timestamp_component() {
        assert_eq!(
            branch_name_with_slug("jowi-dev", "ax-414", "delete-bid-connector"),
            "jowi-dev/ax-414-delete-bid-connector"
        );
    }

    #[test]
    fn slugify_summary_lowercases_and_takes_first_three_content_words() {
        assert_eq!(
            slugify_summary("Delete Bid Connector Integration"),
            Some("delete-bid-connector".to_string())
        );
    }

    #[test]
    fn slugify_summary_splits_on_punctuation_including_hyphens() {
        // A hyphenated summary word splits into separate words rather than
        // collapsing into one — this is what makes the ticket's actual
        // "Delete bid-connector" style summaries produce a 3-word slug
        // instead of a 2-word one.
        assert_eq!(
            slugify_summary("Delete bid-connector"),
            Some("delete-bid-connector".to_string())
        );
    }

    #[test]
    fn slugify_summary_drops_stopwords() {
        assert_eq!(
            slugify_summary("Fix the bug in the login flow"),
            Some("fix-bug-login".to_string())
        );
    }

    #[test]
    fn slugify_summary_falls_back_to_raw_words_when_all_are_stopwords() {
        assert_eq!(slugify_summary("to of for"), Some("to-of-for".to_string()));
    }

    #[test]
    fn slugify_summary_none_for_empty_summary() {
        assert_eq!(slugify_summary(""), None);
    }

    #[test]
    fn slugify_summary_none_for_punctuation_only_summary() {
        assert_eq!(slugify_summary("... !!! ---"), None);
    }

    #[test]
    fn slugify_summary_uses_fewer_than_three_words_when_summary_is_short() {
        assert_eq!(slugify_summary("Fix login"), Some("fix-login".to_string()));
    }

    #[test]
    fn slugify_summary_caps_total_length() {
        let long_summary = "Reticulate the extraordinarily long splines architecture";
        let slug = slugify_summary(long_summary).unwrap();
        assert!(slug.len() <= 40, "slug too long: {slug:?} ({})", slug.len());
        assert!(!slug.ends_with('-'), "slug has dangling dash: {slug:?}");
    }

    #[test]
    fn resolve_branch_collision_returns_candidate_when_free() {
        let result =
            resolve_branch_collision("jowi-dev/ax-414-fix-login", |_: &str| Ok::<bool, ()>(false));
        assert_eq!(result, Ok("jowi-dev/ax-414-fix-login".to_string()));
    }

    #[test]
    fn resolve_branch_collision_appends_dash_two_on_first_collision() {
        let result = resolve_branch_collision("jowi-dev/ax-414-fix-login", |name: &str| {
            Ok::<bool, ()>(name == "jowi-dev/ax-414-fix-login")
        });
        assert_eq!(result, Ok("jowi-dev/ax-414-fix-login-2".to_string()));
    }

    #[test]
    fn resolve_branch_collision_keeps_incrementing_until_free() {
        let taken = ["jowi-dev/ax-414-fix-login", "jowi-dev/ax-414-fix-login-2"];
        let result = resolve_branch_collision("jowi-dev/ax-414-fix-login", |name: &str| {
            Ok::<bool, ()>(taken.contains(&name))
        });
        assert_eq!(result, Ok("jowi-dev/ax-414-fix-login-3".to_string()));
    }

    #[test]
    fn resolve_branch_collision_propagates_exists_error() {
        let result: Result<String, &str> =
            resolve_branch_collision("jowi-dev/ax-414-fix-login", |_: &str| Err("boom"));
        assert_eq!(result, Err("boom"));
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
