//! Pure PR title/body/branch parsing: recovering a Jira issue key from a
//! GitHub pull request, and prefixing a PR title with one.
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
/// [`find_issue_key_with_source`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    /// Found in the PR title (bracketed prefix or bare token).
    Title,
    /// Found in the PR body.
    Body,
    /// Inferred from the branch name.
    Branch,
}

/// Find the Jira issue key (e.g. `PROJ-372`) associated with a pull request.
///
/// Checks, in order, stopping at the first match:
///
/// 1. A `[KEY-123]` prefix on the title.
/// 2. A bare `KEY-123` token anywhere else in the title.
/// 3. A `KEY-123` token anywhere in the body.
/// 4. The branch name, matched case-insensitively against a
///    `key-123`-shaped segment (e.g. `proj-372-desc` or
///    `feature/proj-372-desc`) and normalized to uppercase.
///
/// Returns `None` if no key is found by any of these means.
pub fn find_issue_key(pr: &PrInfo) -> Option<String> {
    find_issue_key_with_source(pr).map(|(key, _)| key)
}

/// Like [`find_issue_key`], but also reports which of the four sources the
/// key was found in.
///
/// Callers that want to trust title/body keys outright but validate a
/// branch-derived key against Jira before relying on it (branch names are
/// inferred, not authored) need to know which case they're in; this is the
/// only way to distinguish them, since [`find_issue_key`] collapses the
/// result to a bare key.
pub fn find_issue_key_with_source(pr: &PrInfo) -> Option<(String, KeySource)> {
    if let Some(key) = title_prefix_key(&pr.title).or_else(|| first_key_match(&pr.title)) {
        return Some((key, KeySource::Title));
    }
    if let Some(key) = first_key_match(&pr.body) {
        return Some((key, KeySource::Body));
    }
    branch_key(&pr.head_ref_name).map(|key| (key, KeySource::Branch))
}

/// Find the first pull request (by ascending PR number, for determinism)
/// resolving to Jira issue key `key`, per [`find_issue_key_with_source`]'s
/// title/body/branch precedence.
///
/// `key` is compared case-insensitively (both sides uppercased), so callers
/// don't need to normalize a ticket key's case before calling this. Returns
/// `None` if no PR in `prs` resolves to `key`.
///
/// Known gap, documented not "fixed": a PR opened by hand with no key in its
/// title or body and a branch name that doesn't match the `key-123` shape
/// won't resolve, the same limitation [`find_issue_key_with_source`] already
/// has everywhere else it's used.
pub fn find_pr_for_ticket<'a>(prs: &'a [PrInfo], key: &str) -> Option<&'a PrInfo> {
    let key = key.to_uppercase();
    let mut matches: Vec<&PrInfo> = prs
        .iter()
        .filter(|pr| {
            find_issue_key_with_source(pr)
                .map(|(found, _)| found.to_uppercase())
                .as_deref()
                == Some(key.as_str())
        })
        .collect();
    matches.sort_by_key(|pr| pr.number);
    matches.into_iter().next()
}

/// Prefix `title` with `[KEY]` unless it is already prefixed with that exact
/// key.
///
/// Idempotent: calling this again on its own output is a no-op. If the key
/// appears elsewhere in the title (not as the prefix), the prefix is still
/// added; the title is never scanned for an existing *unprefixed* occurrence
/// of the key.
pub fn with_issue_key_prefix(title: &str, key: &str) -> String {
    let bracketed = format!("[{key}]");
    if title.starts_with(&bracketed) {
        return title.to_string();
    }
    format!("{bracketed} {title}")
}

/// Match a `[KEY-123]` prefix at the very start of `title`.
fn title_prefix_key(title: &str) -> Option<String> {
    let re = Regex::new(r"^\[([A-Z][A-Z0-9]+-\d+)\]").expect("static regex is valid");
    re.captures(title).map(|caps| caps[1].to_string())
}

/// Find the first `KEY-123`-shaped token in `text`.
fn first_key_match(text: &str) -> Option<String> {
    let re = Regex::new(r"\b([A-Z][A-Z0-9]+-\d+)\b").expect("static regex is valid");
    re.captures(text).map(|caps| caps[1].to_string())
}

/// Find a `key-123`-shaped segment in a branch name (e.g. `proj-372-desc` or
/// `feature/proj-372-desc`), case-insensitively, normalized to uppercase.
fn branch_key(branch: &str) -> Option<String> {
    let re = Regex::new(r"(?i)\b([a-z][a-z0-9]+-\d+)\b").expect("static regex is valid");
    re.captures(branch).map(|caps| caps[1].to_uppercase())
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

    /// (title, body, branch, expected)
    const CASES: &[(&str, &str, &str, Option<&str>)] = &[
        // Precedence 1: bracketed prefix in title wins over everything else.
        (
            "[PROJ-372] Fix the thing",
            "mentions BX-1 too",
            "cx-2-desc",
            Some("PROJ-372"),
        ),
        // Precedence 2: bare token in title wins over body/branch.
        (
            "Fix the thing PROJ-372",
            "mentions BX-1",
            "cx-2-desc",
            Some("PROJ-372"),
        ),
        // Precedence 3: token in body wins over branch.
        (
            "Fix the thing",
            "Resolves PROJ-372",
            "cx-2-desc",
            Some("PROJ-372"),
        ),
        // Precedence 4: plain branch name, lowercase, normalized to uppercase.
        (
            "Fix the thing",
            "no key here",
            "proj-372-desc",
            Some("PROJ-372"),
        ),
        // Precedence 4: branch name with a prefix path segment.
        (
            "Fix the thing",
            "no key here",
            "feature/proj-372-desc",
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
                find_issue_key(&pr),
                expected.map(str::to_string),
                "title={title:?} body={body:?} branch={branch:?}"
            );
        }
    }

    /// (title, body, branch, expected (key, source))
    type SourceCase<'a> = (&'a str, &'a str, &'a str, Option<(&'a str, KeySource)>);

    #[test]
    fn find_issue_key_with_source_table() {
        let cases: &[SourceCase] = &[
            (
                "[PROJ-372] Fix the thing",
                "mentions BX-1 too",
                "cx-2-desc",
                Some(("PROJ-372", KeySource::Title)),
            ),
            (
                "Fix the thing",
                "Resolves PROJ-372",
                "cx-2-desc",
                Some(("PROJ-372", KeySource::Body)),
            ),
            (
                "Fix the thing",
                "no key here",
                "proj-372-desc",
                Some(("PROJ-372", KeySource::Branch)),
            ),
            ("Fix the thing", "no key here", "some-branch-name", None),
        ];

        for (title, body, branch, expected) in cases {
            let pr = pr(title, body, branch);
            assert_eq!(
                find_issue_key_with_source(&pr),
                expected.map(|(key, source)| (key.to_string(), source)),
                "title={title:?} body={body:?} branch={branch:?}"
            );
        }
    }

    #[test]
    fn find_pr_for_ticket_matches_by_title_body_or_branch() {
        let prs = vec![pr("[PROJ-372] Fix the thing", "", "fix-branch")];
        let found = find_pr_for_ticket(&prs, "PROJ-372").expect("expected a match");
        assert_eq!(found.number, 1);
    }

    #[test]
    fn find_pr_for_ticket_no_match_is_none() {
        let prs = vec![pr("Fix the thing", "no key here", "some-branch-name")];
        assert_eq!(find_pr_for_ticket(&prs, "PROJ-372"), None);
    }

    #[test]
    fn find_pr_for_ticket_picks_lowest_number_among_multiple_matches() {
        let mut second = pr("[PROJ-372] Second attempt", "", "proj-372-again");
        second.number = 7;
        let mut first = pr("[PROJ-372] First attempt", "", "proj-372-fix");
        first.number = 3;
        let prs = vec![second, first];
        let found = find_pr_for_ticket(&prs, "PROJ-372").expect("expected a match");
        assert_eq!(found.number, 3);
    }

    #[test]
    fn find_pr_for_ticket_compares_key_case_insensitively() {
        let prs = vec![pr("[PROJ-372] Fix the thing", "", "fix-branch")];
        let found = find_pr_for_ticket(&prs, "proj-372").expect("expected a match");
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
