//! Counting GitHub PR review-thread "bot findings": review comment threads
//! opened by a configured bot login (e.g. `cursor[bot]`), and how many of
//! those remain unresolved.
//!
//! [`ReviewThread`] is the parsed shape [`crate::github::gh_cli::GhCli::pr_review_threads`]
//! returns; [`count_bot_findings`] is the pure counting logic over it, kept
//! separate from the `gh` shell-out so it can be unit tested directly.

/// A GitHub pull request review thread.
///
/// Corresponds to one node of the `reviewThreads` GraphQL connection on a
/// pull request (`isResolved` plus the author of the thread's first
/// comment).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewThread {
    /// Whether the thread has been marked resolved.
    pub is_resolved: bool,
    /// Login of the author of the thread's first comment.
    ///
    /// `None` when the author is null, e.g. a deleted account.
    pub author_login: Option<String>,
}

/// Counts of bot-authored review threads on a pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BotFindingCounts {
    /// Total number of review threads authored by a configured bot login.
    pub total: usize,
    /// Number of those threads that are still unresolved.
    pub unresolved: usize,
}

/// One review submission on a pull request, as returned by `gh api
/// repos/{owner}/{repo}/pulls/{number}/reviews`.
///
/// Unlike [`ReviewThread`], which comes from GraphQL and reports bot logins
/// *without* the `[bot]` suffix, this comes from GitHub's REST API and
/// reports bot logins *with* the suffix (e.g. `cursor[bot]`). See
/// [`bot_login_matches`] for how both forms are handled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrReview {
    /// Login of the user or bot that submitted the review.
    ///
    /// `None` when the author is null, e.g. a deleted account.
    pub author_login: Option<String>,
}

/// Count how many of `threads` were opened by one of `bot_logins`, and how
/// many of those remain unresolved.
///
/// A thread counts as a bot finding when its `author_login` matches any
/// entry in `bot_logins`. Threads with no author (`None`) are never counted.
///
/// ## Matching gotcha: REST vs GraphQL bot login formats
///
/// GitHub's REST API reports bot logins with a trailing `[bot]` suffix
/// (e.g. `cursor[bot]`), which is the familiar form users write in the
/// `review_bots` config setting. GitHub's GraphQL API -- the only API that
/// exposes review thread resolution state, hence the only source
/// `ReviewThread`s come from -- returns bot author logins *without* that
/// suffix (`cursor`, not `cursor[bot]`). A naive exact-match comparison
/// between a `cursor[bot]` config entry and a GraphQL `cursor` author would
/// therefore never match anything.
///
/// To handle this, each configured login is compared against a thread's
/// author two ways, case-insensitively: with a trailing `[bot]` suffix
/// stripped from the config entry, and as an exact match. This makes both
/// the common `cursor[bot]` config form (matched against GraphQL's
/// unsuffixed `cursor`) and a config entry already written without the
/// suffix work correctly, and keeps matching correct if some future API
/// call site returns the suffixed form directly.
pub fn count_bot_findings(threads: &[ReviewThread], bot_logins: &[String]) -> BotFindingCounts {
    let mut total = 0;
    let mut unresolved = 0;

    for thread in threads {
        let Some(author) = &thread.author_login else {
            continue;
        };
        if bot_logins
            .iter()
            .any(|login| bot_login_matches(login, author))
        {
            total += 1;
            if !thread.is_resolved {
                unresolved += 1;
            }
        }
    }

    BotFindingCounts { total, unresolved }
}

/// Whether `author` matches a configured bot `login`, per the suffix-and-case
/// rules documented on [`count_bot_findings`].
///
/// `pub(crate)` (not private) because it's reused across the crate wherever a
/// bot-authored login needs matching against `review_bots` config, e.g.
/// [`bots_have_reviewed`] and `gh_cli`'s finding-details filtering.
pub(crate) fn bot_login_matches(login: &str, author: &str) -> bool {
    let stripped = login.strip_suffix("[bot]").unwrap_or(login);
    stripped.eq_ignore_ascii_case(author) || login.eq_ignore_ascii_case(author)
}

/// True once every configured bot login has at least one review submission
/// on the pull request. Vacuously true when `bot_logins` is empty.
///
/// This is the "bots finished" predicate the review-watch poll loop uses: a
/// bot that reviews and finds nothing posts a review with zero comments, so
/// [`ReviewThread`] data alone can't tell you whether a bot has run at all —
/// only that it left comments if it did. `reviews` (from `gh api
/// repos/{owner}/{repo}/pulls/{number}/reviews`) reports one entry per review
/// submission regardless of comment count, which this checks instead.
pub fn bots_have_reviewed(reviews: &[PrReview], bot_logins: &[String]) -> bool {
    bot_logins.iter().all(|login| {
        reviews.iter().any(|review| {
            review
                .author_login
                .as_deref()
                .is_some_and(|author| bot_login_matches(login, author))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread(is_resolved: bool, author_login: Option<&str>) -> ReviewThread {
        ReviewThread {
            is_resolved,
            author_login: author_login.map(str::to_string),
        }
    }

    fn cursor_bot() -> Vec<String> {
        vec!["cursor[bot]".to_string()]
    }

    #[test]
    fn matches_graphql_unsuffixed_login_against_suffixed_config() {
        let threads = vec![thread(false, Some("cursor"))];
        let counts = count_bot_findings(&threads, &cursor_bot());
        assert_eq!(
            counts,
            BotFindingCounts {
                total: 1,
                unresolved: 1
            }
        );
    }

    #[test]
    fn matches_case_insensitively() {
        let threads = vec![thread(false, Some("Cursor"))];
        let counts = count_bot_findings(&threads, &cursor_bot());
        assert_eq!(counts.total, 1);
    }

    #[test]
    fn matches_exact_suffixed_login_too() {
        let threads = vec![thread(false, Some("cursor[bot]"))];
        let counts = count_bot_findings(&threads, &cursor_bot());
        assert_eq!(counts.total, 1);
    }

    #[test]
    fn non_bot_authors_are_ignored() {
        let threads = vec![thread(false, Some("some-human"))];
        let counts = count_bot_findings(&threads, &cursor_bot());
        assert_eq!(counts, BotFindingCounts::default());
    }

    #[test]
    fn threads_with_no_author_are_ignored() {
        let threads = vec![thread(false, None)];
        let counts = count_bot_findings(&threads, &cursor_bot());
        assert_eq!(counts, BotFindingCounts::default());
    }

    #[test]
    fn counts_resolved_and_unresolved_separately() {
        let threads = vec![
            thread(true, Some("cursor")),
            thread(false, Some("cursor")),
            thread(false, Some("cursor")),
        ];
        let counts = count_bot_findings(&threads, &cursor_bot());
        assert_eq!(
            counts,
            BotFindingCounts {
                total: 3,
                unresolved: 2
            }
        );
    }

    #[test]
    fn empty_threads_is_zero_counts() {
        let counts = count_bot_findings(&[], &cursor_bot());
        assert_eq!(counts, BotFindingCounts::default());
    }

    #[test]
    fn empty_bot_logins_matches_nothing() {
        let threads = vec![thread(false, Some("cursor"))];
        let counts = count_bot_findings(&threads, &[]);
        assert_eq!(counts, BotFindingCounts::default());
    }

    fn review(author_login: Option<&str>) -> PrReview {
        PrReview {
            author_login: author_login.map(str::to_string),
        }
    }

    #[test]
    fn bots_have_reviewed_true_when_every_bot_has_a_review() {
        let reviews = vec![review(Some("cursor[bot]"))];
        assert!(bots_have_reviewed(&reviews, &cursor_bot()));
    }

    #[test]
    fn bots_have_reviewed_false_when_a_configured_bot_is_missing() {
        let reviews = vec![review(Some("some-human"))];
        assert!(!bots_have_reviewed(&reviews, &cursor_bot()));
    }

    #[test]
    fn bots_have_reviewed_false_when_only_some_bots_have_reviewed() {
        let bots = vec!["cursor[bot]".to_string(), "other[bot]".to_string()];
        let reviews = vec![review(Some("cursor[bot]"))];
        assert!(!bots_have_reviewed(&reviews, &bots));
    }

    #[test]
    fn bots_have_reviewed_matches_unsuffixed_login_case_insensitively() {
        let reviews = vec![review(Some("Cursor"))];
        assert!(bots_have_reviewed(&reviews, &cursor_bot()));
    }

    #[test]
    fn bots_have_reviewed_vacuously_true_for_empty_bot_list() {
        assert!(bots_have_reviewed(&[], &[]));
        assert!(bots_have_reviewed(&[review(Some("cursor[bot]"))], &[]));
    }

    #[test]
    fn bots_have_reviewed_false_for_empty_reviews_with_configured_bots() {
        assert!(!bots_have_reviewed(&[], &cursor_bot()));
    }

    #[test]
    fn bots_have_reviewed_ignores_null_author_reviews() {
        let reviews = vec![review(None)];
        assert!(!bots_have_reviewed(&reviews, &cursor_bot()));
    }
}
