//! `tm ready` and `tm ready <KEY>`.
//!
//! `tm ready` (no key) lists tickets assigned to the current user that are
//! ready to pick up: see [`crate::ticketing::ready_tickets`] for the exact
//! candidate query and blocker filter. `tm ready <KEY>` checks one specific
//! ticket (any assignee, any status) via [`crate::ticketing::check_ready`]
//! and exits non-zero if it's blocked, so scripts can branch on it —
//! implemented as [`ReadyCliError::NotReady`] so `main.rs`'s existing
//! error-to-exit-code path handles this without special-casing `tm ready`.

use std::io::Write;

use thiserror::Error;

use crate::jira::client::JiraClient;
use crate::ticketing::{TicketingError, check_ready, ready_tickets};

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
pub fn list(jira: &dyn JiraClient, out: &mut dyn Write) -> Result<(), ReadyCliError> {
    let listing = ready_tickets(jira)?;

    if listing.ready.is_empty() {
        writeln!(out, "No ready tickets.")?;
    } else {
        for issue in &listing.ready {
            writeln!(out, "{}  {}", issue.key, issue.fields.summary)?;
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
pub fn check(jira: &dyn JiraClient, key: &str, out: &mut dyn Write) -> Result<(), ReadyCliError> {
    let normalized = normalize(key)?;
    let result = check_ready(jira, &normalized)?;

    if result.open_blockers.is_empty() {
        writeln!(out, "{normalized} is ready ({})", result.status_name)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jira::fake::FakeJiraClient;
    use crate::jira::types::{
        Issue, IssueFields, IssueLink, IssueLinkType, LinkedIssue, LinkedIssueFields, SearchResult,
        Status, StatusCategory,
    };

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
        let mut out = Vec::new();

        list(&jira, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(
            output,
            "PROJ-1  Summary for PROJ-1\nPROJ-2  Summary for PROJ-2\n"
        );
    }

    #[test]
    fn list_with_no_candidates_prints_no_ready_tickets() {
        let jira = FakeJiraClient::new().with_search_result(search_result(vec![]));
        let mut out = Vec::new();

        list(&jira, &mut out).expect("should succeed");

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
        let mut out = Vec::new();

        list(&jira, &mut out).expect("should succeed");

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
        let mut out = Vec::new();

        list(&jira, &mut out).expect("should succeed");

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
        let mut out = Vec::new();

        list(&jira, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(output, "No ready tickets.\n(2 blocked tickets hidden)\n");
    }

    #[test]
    fn check_ready_ticket_prints_ready_message() {
        let jira = FakeJiraClient::new().with_issue("PROJ-1", issue("PROJ-1"));
        let mut out = Vec::new();

        check(&jira, "proj-1", &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(output, "PROJ-1 is ready (To Do)\n");
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
        let mut out = Vec::new();

        let err = check(&jira, "proj-1", &mut out).expect_err("should fail");

        let rendered = err.to_string();
        assert!(rendered.contains("PROJ-1 is blocked by:"));
        assert!(rendered.contains("PROJ-2 (In Progress): Summary for PROJ-2"));
        assert!(
            !rendered.contains("PROJ-3"),
            "Done blocker should not be listed: {rendered}"
        );
        assert!(out.is_empty(), "nothing should be printed on failure");
    }

    #[test]
    fn check_invalid_key_is_an_actionable_error() {
        let jira = FakeJiraClient::new();
        let mut out = Vec::new();

        let err = check(&jira, "not-a-key!", &mut out).expect_err("should fail");

        match err {
            ReadyCliError::InvalidKey { key } => assert_eq!(key, "not-a-key!"),
            other => panic!("expected InvalidKey, got {other:?}"),
        }
    }

    #[test]
    fn check_not_found_error_passes_through() {
        let jira = FakeJiraClient::new().with_issue_not_found("PROJ-404");
        let mut out = Vec::new();

        let err = check(&jira, "proj-404", &mut out).expect_err("should fail");

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
