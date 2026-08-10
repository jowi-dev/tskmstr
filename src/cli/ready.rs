//! `tm ready` and `tm ready <KEY>`.
//!
//! `tm ready` (no key) lists tickets assigned to the current user that are
//! ready to pick up: see [`crate::ticketing::ready_tickets`] for the exact
//! candidate query and blocker filter. `tm ready <KEY>` checks one specific
//! ticket (any assignee, any status) via [`crate::ticketing::check_ready`]
//! and exits non-zero if it's blocked, so scripts can branch on it —
//! implemented as [`ReadyCliError::NotReady`] so `main.rs`'s existing
//! error-to-exit-code path handles this without special-casing `tm ready`.
//!
//! Both forms also carry a best-effort, ADVISORY annotation of GitHub bot
//! review findings (see [`crate::github::bot_findings`]) on a ready ticket's
//! associated pull request, if any. This is purely informational: it never
//! hides a ticket, changes an exit code, or turns a `gh` failure into an
//! error, since a bot false positive must not freeze a ticket. See
//! [`bot_finding_annotations`] and [`print_bot_finding_note`] for the lookup.

use std::collections::HashMap;
use std::io::Write;

use thiserror::Error;

use crate::github::bot_findings::count_bot_findings;
use crate::github::gh_cli::GhCli;
use crate::github::pr::{PrInfo, find_issue_key};
use crate::jira::client::JiraClient;
use crate::jira::types::Issue;
use crate::ticketing::{TicketingError, check_ready, ready_tickets};

/// Dependencies `tm ready`'s bot-findings annotation needs, alongside the
/// Jira client, bundled to keep `list`/`check`'s arity within this project's
/// convention (see `TicketingContext`).
pub struct ReadyContext<'a> {
    /// Jira client used for the readiness query/check itself.
    pub jira: &'a dyn JiraClient,
    /// `gh` client used for the best-effort bot-findings annotation.
    pub gh: &'a dyn GhCli,
    /// Bot logins configured as [`crate::config::Config::review_bots`].
    pub review_bots: &'a [String],
}

/// Errors surfaced by `tm ready`.
#[derive(Debug, Error)]
pub enum ReadyCliError {
    /// `key` didn't normalize to a valid Jira issue key shape.
    #[error("invalid ticket key `{key}`; expected a Jira key like PROJ-123")]
    InvalidKey {
        /// The key as originally passed on the command line.
        key: String,
    },

    /// `tm ready <KEY>` found `key` has at least one open blocker.
    ///
    /// Carries the full blocker list pre-formatted in `blockers` (one line
    /// per open blocker, `"  OTHER-KEY (status): summary"`) so the `Display`
    /// output the user sees, and that a script capturing stderr sees, names
    /// every blocker without a second lookup.
    #[error("{key} is blocked by:\n{blockers}")]
    NotReady {
        /// The issue key that was checked.
        key: String,
        /// Pre-formatted, newline-joined list of open blocker lines.
        blockers: String,
    },

    /// A Jira API call failed.
    #[error(transparent)]
    Ticketing(#[from] TicketingError),

    /// Writing output failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Normalize `key` the same way `tm ticket` does, mapping the shared
/// [`crate::cli::ticket::TicketCliError::InvalidKey`] into this module's own
/// error type rather than duplicating the regex.
fn normalize(key: &str) -> Result<String, ReadyCliError> {
    super::ticket::normalize_key(key).map_err(|err| match err {
        super::ticket::TicketCliError::InvalidKey { key } => ReadyCliError::InvalidKey { key },
        other => unreachable!("normalize_key only ever returns InvalidKey, got {other:?}"),
    })
}

/// `tm ready` (no key): print tickets assigned to the current user that are
/// ready to pick up, one per line as `KEY  Summary`, in rank order.
///
/// Prints `No ready tickets.` if none are ready. If any candidates were
/// excluded for having an open blocker, appends a final `(N blocked tickets
/// hidden)` line so a filtered list doesn't read as "this is everything
/// assigned to you". Always exits 0.
///
/// Each line also carries a best-effort, advisory bot-findings annotation:
/// when a ready ticket has an open PR (matched by title) with unresolved bot
/// review findings, the line becomes `KEY  Summary  [N unresolved bot
/// findings]` (singular "finding" when `N == 1`). Zero unresolved or no
/// matching PR leaves the line unchanged. See [`bot_finding_annotations`] for
/// how the lookup degrades if `gh` fails.
pub fn list(ctx: &ReadyContext, out: &mut dyn Write) -> Result<(), ReadyCliError> {
    let listing = ready_tickets(ctx.jira)?;

    if listing.ready.is_empty() {
        writeln!(out, "No ready tickets.")?;
    } else {
        let annotations = bot_finding_annotations(ctx, &listing.ready, out)?;
        for issue in &listing.ready {
            match annotations.get(&issue.key) {
                Some(unresolved) if *unresolved > 0 => {
                    let noun = if *unresolved == 1 {
                        "finding"
                    } else {
                        "findings"
                    };
                    writeln!(
                        out,
                        "{}  {}  [{unresolved} unresolved bot {noun}]",
                        issue.key, issue.fields.summary
                    )?;
                }
                _ => writeln!(out, "{}  {}", issue.key, issue.fields.summary)?,
            }
        }
    }

    if listing.hidden_blocked_count > 0 {
        let noun = if listing.hidden_blocked_count == 1 {
            "ticket"
        } else {
            "tickets"
        };
        writeln!(
            out,
            "({} blocked {noun} hidden)",
            listing.hidden_blocked_count
        )?;
    }

    Ok(())
}

/// `tm ready <KEY>`: check whether `key` (any assignee, any status) is ready
/// to pick up.
///
/// Prints `KEY is ready (<status>)` and returns `Ok(())` if `key` has no open
/// blockers. Otherwise returns [`ReadyCliError::NotReady`], whose `Display`
/// prints a `KEY is blocked by:` header followed by one line per open
/// blocker — `main.rs`'s existing error path turns this into a non-zero
/// exit. Done blockers are never listed.
///
/// On the ready path only, also prints a best-effort, advisory bot-findings
/// note (`  note: N unresolved bot findings on PR #<number>`, singular
/// "finding" when `N == 1`) when `key` has an open PR (matched by title)
/// with unresolved bot review findings. This never affects the return value
/// — the command still returns `Ok(())` — and the blocked path never
/// performs this lookup. See [`print_bot_finding_note`] for how the lookup
/// degrades if `gh` fails.
pub fn check(ctx: &ReadyContext, key: &str, out: &mut dyn Write) -> Result<(), ReadyCliError> {
    let normalized = normalize(key)?;
    let result = check_ready(ctx.jira, &normalized)?;

    if result.open_blockers.is_empty() {
        writeln!(out, "{normalized} is ready ({})", result.status_name)?;
        print_bot_finding_note(ctx, &normalized, out)?;
        return Ok(());
    }

    let blockers = result
        .open_blockers
        .iter()
        .map(|b| {
            format!(
                "  {} ({}): {}",
                b.key, b.fields.status.name, b.fields.summary
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    Err(ReadyCliError::NotReady {
        key: normalized,
        blockers,
    })
}

/// For `list`: compute unresolved bot-finding counts for every ticket in
/// `tickets` that has a matching open PR, degrading to a printed warning
/// (once) rather than an error if `gh` fails.
///
/// Calls `gh.pr_list()` once. If it fails, prints
/// `warning: could not check bot findings: {err}` and returns an empty map —
/// the listing is then printed unannotated. Otherwise, for each ticket
/// matched to an open PR (by [`find_issue_key`] against the PR title), calls
/// `gh.pr_review_threads`; a failure there prints the same warning line
/// exactly once (not once per ticket) and simply omits that ticket (and any
/// ticket whose lookup fails afterward) from the returned map, without
/// aborting the remaining lookups.
fn bot_finding_annotations(
    ctx: &ReadyContext,
    tickets: &[Issue],
    out: &mut dyn Write,
) -> Result<HashMap<String, usize>, ReadyCliError> {
    let mut annotations = HashMap::new();

    // `tm ready` is run from inside the repo, so its own process cwd is the
    // right `dir` to shell `gh` against — see `GhCli::pr_list`'s doc
    // comment on why this argument exists at all (`tm pr watch`'s detached,
    // not-necessarily-in-repo case, not this one).
    let cwd = std::env::current_dir()?;

    let prs = match ctx.gh.pr_list(&cwd) {
        Ok(prs) => prs,
        Err(err) => {
            writeln!(out, "warning: could not check bot findings: {err}")?;
            return Ok(annotations);
        }
    };

    let mut warned = false;
    for issue in tickets {
        let Some(number) = matching_pr_number(&prs, &issue.key) else {
            continue;
        };
        match ctx.gh.pr_review_threads(&cwd, number) {
            Ok(threads) => {
                let counts = count_bot_findings(&threads, ctx.review_bots);
                if counts.unresolved > 0 {
                    annotations.insert(issue.key.clone(), counts.unresolved);
                }
            }
            Err(err) => {
                if !warned {
                    writeln!(out, "warning: could not check bot findings: {err}")?;
                    warned = true;
                }
            }
        }
    }

    Ok(annotations)
}

/// For `check`: print a `  note: N unresolved bot findings on PR #<number>`
/// line (singular "finding" when `N == 1`) if `key` has a matching open PR
/// with unresolved bot review findings.
///
/// Degrades the same way as [`bot_finding_annotations`]: a `pr_list` or
/// `pr_review_threads` failure prints
/// `warning: could not check bot findings: {err}` instead, never an error.
fn print_bot_finding_note(
    ctx: &ReadyContext,
    key: &str,
    out: &mut dyn Write,
) -> Result<(), ReadyCliError> {
    let cwd = std::env::current_dir()?;

    let prs = match ctx.gh.pr_list(&cwd) {
        Ok(prs) => prs,
        Err(err) => {
            writeln!(out, "warning: could not check bot findings: {err}")?;
            return Ok(());
        }
    };

    let Some(number) = matching_pr_number(&prs, key) else {
        return Ok(());
    };

    match ctx.gh.pr_review_threads(&cwd, number) {
        Ok(threads) => {
            let counts = count_bot_findings(&threads, ctx.review_bots);
            if counts.unresolved > 0 {
                let noun = if counts.unresolved == 1 {
                    "finding"
                } else {
                    "findings"
                };
                writeln!(
                    out,
                    "  note: {} unresolved bot {noun} on PR #{number}",
                    counts.unresolved
                )?;
            }
        }
        Err(err) => {
            writeln!(out, "warning: could not check bot findings: {err}")?;
        }
    }

    Ok(())
}

/// Find the number of the open PR (from `gh pr list`) resolving to `key`,
/// reusing [`find_issue_key`] — the same title/body/branch extraction `tm pr
/// status` uses — rather than re-implementing key parsing.
fn matching_pr_number(prs: &[PrInfo], key: &str) -> Option<u64> {
    prs.iter()
        .find(|pr| find_issue_key(pr).as_deref() == Some(key))
        .map(|pr| pr.number)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::bot_findings::ReviewThread;
    use crate::github::gh_cli::{FakeGhCli, GhError};
    use crate::jira::fake::FakeJiraClient;
    use crate::jira::types::{
        Issue, IssueFields, IssueLink, IssueLinkType, LinkedIssue, LinkedIssueFields, SearchResult,
        Status, StatusCategory,
    };

    /// The default `review_bots` config: `["cursor[bot]"]`.
    fn cursor_bot() -> Vec<String> {
        vec!["cursor[bot]".to_string()]
    }

    fn ready_ctx<'a>(
        jira: &'a FakeJiraClient,
        gh: &'a FakeGhCli,
        review_bots: &'a [String],
    ) -> ReadyContext<'a> {
        ReadyContext {
            jira,
            gh,
            review_bots,
        }
    }

    fn pr_summary(number: u64, title: &str) -> PrInfo {
        PrInfo {
            number,
            url: String::new(),
            title: title.to_string(),
            body: String::new(),
            head_ref_name: String::new(),
        }
    }

    fn review_thread(is_resolved: bool, author_login: &str) -> ReviewThread {
        ReviewThread {
            is_resolved,
            author_login: Some(author_login.to_string()),
        }
    }

    fn issue(key: &str) -> Issue {
        Issue {
            key: key.to_string(),
            fields: IssueFields {
                summary: format!("Summary for {key}"),
                status: Status {
                    name: "To Do".to_string(),
                    status_category: StatusCategory {
                        key: "new".to_string(),
                    },
                },
                description: None,
                assignee: None,
                issue_links: vec![],
            },
        }
    }

    fn blocks_link_type() -> IssueLinkType {
        IssueLinkType {
            name: "Blocks".to_string(),
            inward: "is blocked by".to_string(),
            outward: "blocks".to_string(),
        }
    }

    fn linked_issue(key: &str, status_name: &str, status_category_key: &str) -> LinkedIssue {
        LinkedIssue {
            key: key.to_string(),
            fields: LinkedIssueFields {
                summary: format!("Summary for {key}"),
                status: Status {
                    name: status_name.to_string(),
                    status_category: StatusCategory {
                        key: status_category_key.to_string(),
                    },
                },
            },
        }
    }

    fn search_result(issues: Vec<Issue>) -> SearchResult {
        SearchResult {
            issues,
            next_page_token: None,
        }
    }

    #[test]
    fn list_happy_path_prints_key_and_summary_per_line() {
        let jira = FakeJiraClient::new()
            .with_search_result(search_result(vec![issue("PROJ-1"), issue("PROJ-2")]));
        let gh = FakeGhCli::new();
        let bots = cursor_bot();
        let mut out = Vec::new();

        list(&ready_ctx(&jira, &gh, &bots), &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(
            output,
            "PROJ-1  Summary for PROJ-1\nPROJ-2  Summary for PROJ-2\n"
        );
    }

    #[test]
    fn list_with_no_candidates_prints_no_ready_tickets() {
        let jira = FakeJiraClient::new().with_search_result(search_result(vec![]));
        let gh = FakeGhCli::new();
        let bots = cursor_bot();
        let mut out = Vec::new();

        list(&ready_ctx(&jira, &gh, &bots), &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(output, "No ready tickets.\n");
    }

    #[test]
    fn list_appends_hidden_count_when_candidates_are_blocked() {
        let mut blocked = issue("PROJ-2");
        blocked.fields.issue_links = vec![IssueLink {
            id: "10001".to_string(),
            link_type: blocks_link_type(),
            inward_issue: Some(linked_issue("PROJ-9", "In Progress", "indeterminate")),
            outward_issue: None,
        }];
        let jira =
            FakeJiraClient::new().with_search_result(search_result(vec![issue("PROJ-1"), blocked]));
        let gh = FakeGhCli::new();
        let bots = cursor_bot();
        let mut out = Vec::new();

        list(&ready_ctx(&jira, &gh, &bots), &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(
            output,
            "PROJ-1  Summary for PROJ-1\n(1 blocked ticket hidden)\n"
        );
    }

    #[test]
    fn list_all_blocked_prints_no_ready_tickets_then_hidden_count() {
        let mut blocked = issue("PROJ-1");
        blocked.fields.issue_links = vec![IssueLink {
            id: "10001".to_string(),
            link_type: blocks_link_type(),
            inward_issue: Some(linked_issue("PROJ-9", "In Progress", "indeterminate")),
            outward_issue: None,
        }];
        let jira = FakeJiraClient::new().with_search_result(search_result(vec![blocked]));
        let gh = FakeGhCli::new();
        let bots = cursor_bot();
        let mut out = Vec::new();

        list(&ready_ctx(&jira, &gh, &bots), &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(output, "No ready tickets.\n(1 blocked ticket hidden)\n");
    }

    #[test]
    fn list_hidden_count_pluralizes_above_one() {
        let blocked = |key: &str| {
            let mut i = issue(key);
            i.fields.issue_links = vec![IssueLink {
                id: "10001".to_string(),
                link_type: blocks_link_type(),
                inward_issue: Some(linked_issue("PROJ-9", "In Progress", "indeterminate")),
                outward_issue: None,
            }];
            i
        };
        let jira = FakeJiraClient::new()
            .with_search_result(search_result(vec![blocked("PROJ-1"), blocked("PROJ-2")]));
        let gh = FakeGhCli::new();
        let bots = cursor_bot();
        let mut out = Vec::new();

        list(&ready_ctx(&jira, &gh, &bots), &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(output, "No ready tickets.\n(2 blocked tickets hidden)\n");
    }

    #[test]
    fn list_annotates_ticket_with_matched_pr_and_unresolved_bot_findings() {
        let jira = FakeJiraClient::new()
            .with_search_result(search_result(vec![issue("PROJ-1"), issue("PROJ-2")]));
        let gh = FakeGhCli::new()
            .with_pr_list(Ok(vec![
                pr_summary(42, "[PROJ-1] Fix the thing"),
                pr_summary(43, "[PROJ-2] Add the widget"),
            ]))
            .with_review_threads(
                42,
                Ok(vec![
                    review_thread(false, "cursor"),
                    review_thread(false, "cursor"),
                    review_thread(true, "cursor"),
                ]),
            )
            .with_review_threads(43, Ok(vec![review_thread(false, "cursor")]));
        let bots = cursor_bot();
        let mut out = Vec::new();

        list(&ready_ctx(&jira, &gh, &bots), &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(
            output,
            "PROJ-1  Summary for PROJ-1  [2 unresolved bot findings]\n\
             PROJ-2  Summary for PROJ-2  [1 unresolved bot finding]\n"
        );
    }

    #[test]
    fn list_leaves_line_unchanged_when_zero_unresolved_bot_findings() {
        let jira = FakeJiraClient::new().with_search_result(search_result(vec![issue("PROJ-1")]));
        let gh = FakeGhCli::new()
            .with_pr_list(Ok(vec![pr_summary(42, "[PROJ-1] Fix the thing")]))
            .with_review_threads(42, Ok(vec![review_thread(true, "cursor")]));
        let bots = cursor_bot();
        let mut out = Vec::new();

        list(&ready_ctx(&jira, &gh, &bots), &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(output, "PROJ-1  Summary for PROJ-1\n");
    }

    #[test]
    fn list_leaves_line_unchanged_when_no_matching_pr() {
        let jira = FakeJiraClient::new().with_search_result(search_result(vec![issue("PROJ-1")]));
        let gh = FakeGhCli::new().with_pr_list(Ok(vec![pr_summary(42, "Unrelated PR")]));
        let bots = cursor_bot();
        let mut out = Vec::new();

        list(&ready_ctx(&jira, &gh, &bots), &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(output, "PROJ-1  Summary for PROJ-1\n");
        assert!(!output.contains("warning"));
    }

    #[test]
    fn list_pr_list_failure_warns_once_and_prints_unannotated_list() {
        let jira = FakeJiraClient::new()
            .with_search_result(search_result(vec![issue("PROJ-1"), issue("PROJ-2")]));
        let gh = FakeGhCli::new().with_pr_list(Err(GhError::Command {
            command: "gh pr list".to_string(),
            exit_code: Some(1),
            stderr: "boom".to_string(),
        }));
        let bots = cursor_bot();
        let mut out = Vec::new();

        list(&ready_ctx(&jira, &gh, &bots), &mut out).expect("should still succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(
            output,
            "warning: could not check bot findings: `gh pr list` failed (exit Some(1)): boom\n\
             PROJ-1  Summary for PROJ-1\n\
             PROJ-2  Summary for PROJ-2\n"
        );
    }

    #[test]
    fn list_review_threads_failure_for_one_ticket_warns_once_and_still_annotates_the_other() {
        let jira = FakeJiraClient::new()
            .with_search_result(search_result(vec![issue("PROJ-1"), issue("PROJ-2")]));
        let gh = FakeGhCli::new()
            .with_pr_list(Ok(vec![
                pr_summary(42, "[PROJ-1] Fix the thing"),
                pr_summary(43, "[PROJ-2] Add the widget"),
            ]))
            .with_review_threads(
                42,
                Err(GhError::Command {
                    command: "gh api graphql".to_string(),
                    exit_code: Some(1),
                    stderr: "boom".to_string(),
                }),
            )
            .with_review_threads(43, Ok(vec![review_thread(false, "cursor")]));
        let bots = cursor_bot();
        let mut out = Vec::new();

        list(&ready_ctx(&jira, &gh, &bots), &mut out).expect("should still succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(
            output.matches("warning:").count(),
            1,
            "expected exactly one warning line: {output}"
        );
        assert!(output.contains("PROJ-1  Summary for PROJ-1\n"));
        assert!(output.contains("PROJ-2  Summary for PROJ-2  [1 unresolved bot finding]\n"));
    }

    #[test]
    fn check_ready_ticket_prints_ready_message() {
        let jira = FakeJiraClient::new().with_issue("PROJ-1", issue("PROJ-1"));
        let gh = FakeGhCli::new();
        let bots = cursor_bot();
        let mut out = Vec::new();

        check(&ready_ctx(&jira, &gh, &bots), "proj-1", &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(output, "PROJ-1 is ready (To Do)\n");
    }

    #[test]
    fn check_ready_ticket_with_unresolved_bot_findings_prints_note() {
        let jira = FakeJiraClient::new().with_issue("PROJ-1", issue("PROJ-1"));
        let gh = FakeGhCli::new()
            .with_pr_list(Ok(vec![pr_summary(42, "[PROJ-1] Fix the thing")]))
            .with_review_threads(42, Ok(vec![review_thread(false, "cursor")]));
        let bots = cursor_bot();
        let mut out = Vec::new();

        check(&ready_ctx(&jira, &gh, &bots), "proj-1", &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(
            output,
            "PROJ-1 is ready (To Do)\n  note: 1 unresolved bot finding on PR #42\n"
        );
    }

    #[test]
    fn check_blocked_ticket_is_an_error_listing_open_blockers_and_excluding_done() {
        let mut i = issue("PROJ-1");
        i.fields.issue_links = vec![
            IssueLink {
                id: "10001".to_string(),
                link_type: blocks_link_type(),
                inward_issue: Some(linked_issue("PROJ-2", "In Progress", "indeterminate")),
                outward_issue: None,
            },
            IssueLink {
                id: "10001".to_string(),
                link_type: blocks_link_type(),
                inward_issue: Some(linked_issue("PROJ-3", "Done", "done")),
                outward_issue: None,
            },
        ];
        let jira = FakeJiraClient::new().with_issue("PROJ-1", i);
        let gh = FakeGhCli::new().with_pr_list(Err(GhError::Command {
            command: "gh pr list".to_string(),
            exit_code: Some(1),
            stderr: "should never be called".to_string(),
        }));
        let bots = cursor_bot();
        let mut out = Vec::new();

        let err =
            check(&ready_ctx(&jira, &gh, &bots), "proj-1", &mut out).expect_err("should fail");

        let rendered = err.to_string();
        assert!(rendered.contains("PROJ-1 is blocked by:"));
        assert!(rendered.contains("PROJ-2 (In Progress): Summary for PROJ-2"));
        assert!(
            !rendered.contains("PROJ-3"),
            "Done blocker should not be listed: {rendered}"
        );
        assert!(out.is_empty(), "nothing should be printed on failure");
        assert!(
            !String::from_utf8(out.clone()).unwrap().contains("warning"),
            "the blocked path must never perform a bot-findings lookup"
        );
    }

    #[test]
    fn check_invalid_key_is_an_actionable_error() {
        let jira = FakeJiraClient::new();
        let gh = FakeGhCli::new();
        let bots = cursor_bot();
        let mut out = Vec::new();

        let err =
            check(&ready_ctx(&jira, &gh, &bots), "not-a-key!", &mut out).expect_err("should fail");

        match err {
            ReadyCliError::InvalidKey { key } => assert_eq!(key, "not-a-key!"),
            other => panic!("expected InvalidKey, got {other:?}"),
        }
    }

    #[test]
    fn check_not_found_error_passes_through() {
        let jira = FakeJiraClient::new().with_issue_not_found("PROJ-404");
        let gh = FakeGhCli::new();
        let bots = cursor_bot();
        let mut out = Vec::new();

        let err =
            check(&ready_ctx(&jira, &gh, &bots), "proj-404", &mut out).expect_err("should fail");

        match err {
            ReadyCliError::Ticketing(TicketingError::Jira(
                crate::jira::client::JiraError::NotFound { key },
            )) => {
                assert_eq!(key, "PROJ-404")
            }
            other => panic!("expected Jira NotFound, got {other:?}"),
        }
    }
}
