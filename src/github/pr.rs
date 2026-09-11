//! Pure PR title/body/branch parsing: recovering a ticket key from a
//! GitHub pull request, and prefixing a PR title with one.
//!
//! Every extraction takes an `accept` predicate (in practice
//! `TicketProvider::is_ticket_key`) deciding which key-shaped tokens count
//! as ticket keys for the configured backend: `ADR-0006` in a PR body is
//! key-shaped but is a doc label, not a ticket, and scraping it associated
//! a PR with the wrong issue (GitHub issue #35). Rejected tokens are
//! skipped, not fatal — the scan moves on to the next token and then the
//! next source.
//!
//! No I/O lives here; [`crate::github::gh_cli`] is responsible for actually
//! fetching a [`PrInfo`] from `gh`.

use regex::Regex;
use serde::Deserialize;

/// A GitHub pull request, as returned by
/// `gh pr view --json number,url,title,body,headRefName`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrInfo {
    /// Pull request number.
    pub number: u64,
    /// Web URL of the pull request.
    pub url: String,
    /// Pull request title.
    pub title: String,
    /// Pull request body (description).
    pub body: String,
    /// Name of the branch the PR is opened from.
    pub head_ref_name: String,
}

/// Which part of a pull request an issue key resolved by
/// [`issue_key_candidates`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    /// Found in the PR title (bracketed prefix or bare token).
    Title,
    /// Found in the PR body.
    Body,
    /// Inferred from the branch name.
    Branch,
}

/// Find the ticket key (e.g. `PROJ-372`) associated with a pull request:
/// the first entry of [`issue_key_candidates`], or `None` if no source
/// yields an accepted key.
pub fn find_issue_key(pr: &PrInfo, accept: &dyn Fn(&str) -> bool) -> Option<String> {
    issue_key_candidates(pr, accept)
        .into_iter()
        .next()
        .map(|(key, _)| key)
}

/// Collect the accepted ticket-key candidate from each part of a pull
/// request, in precedence order (at most one entry per source):
///
/// 1. [`KeySource::Title`]: a `[KEY-123]` prefix, or failing that a bare
///    `KEY-123` token anywhere in the title.
/// 2. [`KeySource::Branch`]: the branch name, matched case-insensitively
///    against a `key-123`-shaped segment (e.g. `proj-372-desc` or
///    `feature/proj-372-desc`) and normalized to uppercase.
/// 3. [`KeySource::Body`]: a `KEY-123` token anywhere in the body.
///
/// The branch outranks the body deliberately: a branch is named for the
/// ticket it was cut for, while body prose freely quotes other tickets and
/// doc labels (GitHub issue #35). Tokens `accept` rejects are skipped in
/// favor of the next token in the same source.
///
/// Callers that trust title/body keys outright but validate a
/// branch-derived key against the ticket backend before relying on it
/// (branch names are inferred, not authored — see
/// [`crate::ticketing::resolve_existing_key`]) walk this list; callers
/// that just need "the" key take the first entry via [`find_issue_key`].
pub fn issue_key_candidates(
    pr: &PrInfo,
    accept: &dyn Fn(&str) -> bool,
) -> Vec<(String, KeySource)> {
    let mut candidates = Vec::new();
    if let Some(key) = title_prefix_key(&pr.title)
        .filter(|key| accept(key))
        .or_else(|| first_key_match(&pr.title, accept))
    {
        candidates.push((key, KeySource::Title));
    }
    if let Some(key) = branch_key(&pr.head_ref_name, accept) {
        candidates.push((key, KeySource::Branch));
    }
    if let Some(key) = first_key_match(&pr.body, accept) {
        candidates.push((key, KeySource::Body));
    }
    candidates
}

/// Find the first pull request (by ascending PR number, for determinism)
/// resolving to ticket key `key`, per [`issue_key_candidates`]'s
/// title/branch/body precedence.
///
/// `key` is compared case-insensitively (both sides uppercased), so callers
/// don't need to normalize a ticket key's case before calling this. Returns
/// `None` if no PR in `prs` resolves to `key`.
///
/// Known gap, documented not "fixed": a PR opened by hand with no key in its
/// title or body and a branch name that doesn't match the `key-123` shape
/// won't resolve, the same limitation [`issue_key_candidates`] already
/// has everywhere else it's used.
pub fn find_pr_for_ticket<'a>(
    prs: &'a [PrInfo],
    key: &str,
    accept: &dyn Fn(&str) -> bool,
) -> Option<&'a PrInfo> {
    let key = key.to_uppercase();
    let mut matches: Vec<&PrInfo> = prs
        .iter()
        .filter(|pr| {
            find_issue_key(pr, accept)
                .map(|found| found.to_uppercase())
                .as_deref()
                == Some(key.as_str())
        })
        .collect();
    matches.sort_by_key(|pr| pr.number);
    matches.into_iter().next()
}

/// Prefix `title` with `[KEY]`, replacing any existing key-shaped `[...]`
/// prefixes rather than stacking a second one: re-associating a PR with a
/// different ticket (`tm ticket <KEY>` after a wrong scrape, see GitHub
/// issue #35) must not leave `[GH-30] [ADR-0006] ...` behind. Non-key
/// brackets (`[WIP]`) are left alone.
///
/// Idempotent: calling this again on its own output is a no-op. If the key
/// appears elsewhere in the title (not as the prefix), the prefix is still
/// added; the title is never scanned for an existing *unprefixed* occurrence
/// of the key.
pub fn with_issue_key_prefix(title: &str, key: &str) -> String {
    let mut rest = title;
    while let Some(prefix_key) = title_prefix_key(rest) {
        rest = rest[prefix_key.len() + 2..].trim_start();
    }
    format!("[{key}] {rest}")
}

/// Match a `[KEY-123]` prefix at the very start of `title`. Shape-only, no
/// `accept` filtering: [`with_issue_key_prefix`] strips *any* key-shaped
/// prefix (a stale one is exactly what needs replacing);
/// [`issue_key_candidates`] applies its own `accept` on top.
fn title_prefix_key(title: &str) -> Option<String> {
    let re = Regex::new(r"^\[([A-Z][A-Z0-9]+-\d+)\]").expect("static regex is valid");
    re.captures(title).map(|caps| caps[1].to_string())
}

/// Find the first `KEY-123`-shaped token in `text` that `accept` allows,
/// skipping rejected tokens (e.g. an `ADR-0006` doc label ahead of the
/// real key).
fn first_key_match(text: &str, accept: &dyn Fn(&str) -> bool) -> Option<String> {
    let re = Regex::new(r"\b([A-Z][A-Z0-9]+-\d+)\b").expect("static regex is valid");
    re.find_iter(text)
        .map(|m| m.as_str().to_string())
        .find(|key| accept(key))
}

/// Find the first `key-123`-shaped segment in a branch name (e.g.
/// `proj-372-desc` or `feature/proj-372-desc`) that `accept` allows,
/// matched case-insensitively and normalized to uppercase before the
/// `accept` check (so `jowi-dev/gh-30-lane` yields `GH-30`, skipping any
/// earlier segment `accept` rejects).
fn branch_key(branch: &str, accept: &dyn Fn(&str) -> bool) -> Option<String> {
    let re = Regex::new(r"(?i)\b([a-z][a-z0-9]+-\d+)\b").expect("static regex is valid");
    re.find_iter(branch)
        .map(|m| m.as_str().to_uppercase())
        .find(|key| accept(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(title: &str, body: &str, branch: &str) -> PrInfo {
        PrInfo {
            number: 1,
            url: "https://github.com/example/repo/pull/1".to_string(),
            title: title.to_string(),
            body: body.to_string(),
            head_ref_name: branch.to_string(),
        }
    }

    /// Accept every key-shaped token — the loosest possible backend.
    fn any(_key: &str) -> bool {
        true
    }

    /// The github backend's key scheme: `GH-<number>` only.
    fn gh_only(key: &str) -> bool {
        key.strip_prefix("GH-")
            .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
    }

    /// (title, body, branch, expected)
    const CASES: &[(&str, &str, &str, Option<&str>)] = &[
        // Precedence 1: bracketed prefix in title wins over everything else.
        (
            "[PROJ-372] Fix the thing",
            "mentions BX-1 too",
            "cx-2-desc",
            Some("PROJ-372"),
        ),
        // Precedence 2: bare token in title wins over branch/body.
        (
            "Fix the thing PROJ-372",
            "mentions BX-1",
            "cx-2-desc",
            Some("PROJ-372"),
        ),
        // Precedence 3: branch outranks body prose (GitHub issue #35).
        (
            "Fix the thing",
            "Resolves PROJ-372",
            "cx-2-desc",
            Some("CX-2"),
        ),
        // Precedence 3: plain branch name, lowercase, normalized to uppercase.
        (
            "Fix the thing",
            "no key here",
            "proj-372-desc",
            Some("PROJ-372"),
        ),
        // Precedence 3: branch name with a prefix path segment.
        (
            "Fix the thing",
            "no key here",
            "feature/proj-372-desc",
            Some("PROJ-372"),
        ),
        // Precedence 4: body is the last resort.
        (
            "Fix the thing",
            "Resolves PROJ-372",
            "some-branch-name",
            Some("PROJ-372"),
        ),
        // No match anywhere.
        ("Fix the thing", "no key here", "some-branch-name", None),
        // Multi-key in title picks the first occurrence.
        (
            "Fix PROJ-372 also touches BX-1",
            "irrelevant",
            "cx-2-desc",
            Some("PROJ-372"),
        ),
    ];

    #[test]
    fn find_issue_key_table() {
        for (title, body, branch, expected) in CASES {
            let pr = pr(title, body, branch);
            assert_eq!(
                find_issue_key(&pr, &any),
                expected.map(str::to_string),
                "title={title:?} body={body:?} branch={branch:?}"
            );
        }
    }

    /// The GitHub issue #35 repro: a PR body mentioning `ADR-0006` must not
    /// outrank the `gh-30` branch under the github backend's key scheme.
    #[test]
    fn find_issue_key_skips_doc_labels_the_backend_rejects() {
        let pr = pr(
            "Offer agent-assisted lane setup from tm init",
            "Closes #30.\n\nRecorded as ADR-0006.",
            "jowi-dev/gh-30-tm-init-lane",
        );
        assert_eq!(find_issue_key(&pr, &gh_only), Some("GH-30".to_string()));
    }

    #[test]
    fn find_issue_key_skips_rejected_tokens_within_one_source() {
        // ADR-0006 comes first in the body, but the scan moves on to GH-30
        // rather than stopping at the rejected token.
        let pr = pr(
            "Fix the thing",
            "Per ADR-0006 this closes GH-30.",
            "some-branch-name",
        );
        assert_eq!(find_issue_key(&pr, &gh_only), Some("GH-30".to_string()));
    }

    #[test]
    fn find_issue_key_rejected_title_prefix_falls_back_to_bare_title_token() {
        let pr = pr("[ADR-0006] Fix GH-30 thing", "", "some-branch-name");
        assert_eq!(find_issue_key(&pr, &gh_only), Some("GH-30".to_string()));
    }

    #[test]
    fn issue_key_candidates_orders_title_branch_body() {
        let pr = pr("[PROJ-372] Fix the thing", "mentions BX-1 too", "cx-2-desc");
        assert_eq!(
            issue_key_candidates(&pr, &any),
            vec![
                ("PROJ-372".to_string(), KeySource::Title),
                ("CX-2".to_string(), KeySource::Branch),
                ("BX-1".to_string(), KeySource::Body),
            ]
        );
    }

    #[test]
    fn issue_key_candidates_empty_when_nothing_matches() {
        let pr = pr("Fix the thing", "no key here", "some-branch-name");
        assert_eq!(issue_key_candidates(&pr, &any), vec![]);
    }

    #[test]
    fn find_pr_for_ticket_matches_by_title_branch_or_body() {
        let prs = vec![pr("[PROJ-372] Fix the thing", "", "fix-branch")];
        let found = find_pr_for_ticket(&prs, "PROJ-372", &any).expect("expected a match");
        assert_eq!(found.number, 1);
    }

    #[test]
    fn find_pr_for_ticket_no_match_is_none() {
        let prs = vec![pr("Fix the thing", "no key here", "some-branch-name")];
        assert_eq!(find_pr_for_ticket(&prs, "PROJ-372", &any), None);
    }

    #[test]
    fn find_pr_for_ticket_picks_lowest_number_among_multiple_matches() {
        let mut second = pr("[PROJ-372] Second attempt", "", "proj-372-again");
        second.number = 7;
        let mut first = pr("[PROJ-372] First attempt", "", "proj-372-fix");
        first.number = 3;
        let prs = vec![second, first];
        let found = find_pr_for_ticket(&prs, "PROJ-372", &any).expect("expected a match");
        assert_eq!(found.number, 3);
    }

    #[test]
    fn find_pr_for_ticket_compares_key_case_insensitively() {
        let prs = vec![pr("[PROJ-372] Fix the thing", "", "fix-branch")];
        let found = find_pr_for_ticket(&prs, "proj-372", &any).expect("expected a match");
        assert_eq!(found.number, 1);
    }

    #[test]
    fn find_pr_for_ticket_is_not_masked_by_a_rejected_body_token() {
        // Body mentions ADR-0006 ahead of anything else; the branch still
        // resolves the PR for GH-30 under the github key scheme.
        let prs = vec![pr(
            "Offer agent-assisted lane setup",
            "See ADR-0006.",
            "jowi-dev/gh-30-tm-init-lane",
        )];
        let found = find_pr_for_ticket(&prs, "GH-30", &gh_only).expect("expected a match");
        assert_eq!(found.number, 1);
    }

    #[test]
    fn with_issue_key_prefix_adds_prefix() {
        assert_eq!(
            with_issue_key_prefix("Fix the thing", "PROJ-372"),
            "[PROJ-372] Fix the thing"
        );
    }

    #[test]
    fn with_issue_key_prefix_is_idempotent() {
        let once = with_issue_key_prefix("Fix the thing", "PROJ-372");
        let twice = with_issue_key_prefix(&once, "PROJ-372");
        assert_eq!(once, twice);
    }

    #[test]
    fn with_issue_key_prefix_replaces_stale_key_prefix() {
        assert_eq!(
            with_issue_key_prefix("[ADR-0006] Fix the thing", "GH-30"),
            "[GH-30] Fix the thing"
        );
    }

    #[test]
    fn with_issue_key_prefix_collapses_stacked_key_prefixes() {
        assert_eq!(
            with_issue_key_prefix("[GH-30] [ADR-0006] Fix the thing", "GH-30"),
            "[GH-30] Fix the thing"
        );
    }

    #[test]
    fn with_issue_key_prefix_keeps_non_key_brackets() {
        assert_eq!(
            with_issue_key_prefix("[WIP] Fix the thing", "PROJ-372"),
            "[PROJ-372] [WIP] Fix the thing"
        );
    }

    #[test]
    fn with_issue_key_prefix_never_double_prefixes_when_key_appears_elsewhere() {
        // The key appears in the title already, but not as the prefix; the
        // prefix is still added exactly once.
        let title = "Fix PROJ-372 for real this time";
        let prefixed = with_issue_key_prefix(title, "PROJ-372");
        assert_eq!(prefixed, "[PROJ-372] Fix PROJ-372 for real this time");
        // Re-applying is still idempotent.
        assert_eq!(with_issue_key_prefix(&prefixed, "PROJ-372"), prefixed);
    }
}
