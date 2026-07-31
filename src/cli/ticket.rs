//! `tm ticket <KEY>`.

use std::io::Write;

use regex::Regex;
use thiserror::Error;

use crate::ticketing::{TicketingContext, TicketingError, associate_ticket};

/// Errors surfaced by `tm ticket`.
#[derive(Debug, Error)]
pub enum TicketCliError {
    /// `key` didn't normalize to a valid Jira issue key shape.
    #[error("invalid ticket key `{key}`; expected a Jira key like PROJ-123")]
    InvalidKey {
        /// The key as originally passed on the command line.
        key: String,
    },

    /// Association with the current branch's pull request failed.
    #[error(transparent)]
    Ticketing(#[from] TicketingError),

    /// Writing output failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// `tm ticket <KEY>`: normalize and validate `key`, then associate it with
/// the pull request open for the current branch.
pub fn run(ctx: &TicketingContext, key: &str, out: &mut dyn Write) -> Result<(), TicketCliError> {
    let normalized = normalize_key(key)?;
    let outcome = associate_ticket(ctx, &normalized)?;

    writeln!(out, "{}", outcome.issue_url)?;
    writeln!(
        out,
        "Title {}",
        if outcome.title_updated {
            "updated"
        } else {
            "already up to date"
        }
    )?;
    writeln!(
        out,
        "Remote link {}",
        if outcome.remote_link_added {
            "added"
        } else {
            "not added"
        }
    )?;

    Ok(())
}

/// Uppercase `key` and validate it looks like a Jira issue key
/// (`^[A-Z][A-Z0-9]+-\d+$`).
fn normalize_key(key: &str) -> Result<String, TicketCliError> {
    let upper = key.to_uppercase();
    let re = Regex::new(r"^[A-Z][A-Z0-9]+-\d+$").expect("static regex is valid");
    if re.is_match(&upper) {
        Ok(upper)
    } else {
        Err(TicketCliError::InvalidKey {
            key: key.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::github::gh_cli::FakeGhCli;
    use crate::github::pr::PrInfo;
    use crate::jira::client::JiraError;
    use crate::jira::fake::FakeJiraClient;
    use crate::jira::types::{Issue, IssueFields, Status, StatusCategory};

    fn issue(key: &str) -> Issue {
        Issue {
            key: key.to_string(),
            fields: IssueFields {
                summary: "Fix the thing".to_string(),
                status: Status {
                    name: "To Do".to_string(),
                    status_category: StatusCategory {
                        key: "new".to_string(),
                    },
                },
                description: None,
                assignee: None,
            },
        }
    }

    fn pr() -> PrInfo {
        PrInfo {
            number: 42,
            url: "https://github.com/example/repo/pull/42".to_string(),
            title: "Fix the thing".to_string(),
            body: String::new(),
            head_ref_name: "proj-372-fix".to_string(),
        }
    }

    fn config() -> Config {
        Config {
            jira_base_url: "https://example.atlassian.net".to_string(),
            jira_email: "dev@example.com".to_string(),
            default_project_key: "PROJ".to_string(),
            default_assignee_account_id: None,
            status_on_pr: None,
        }
    }

    #[test]
    fn happy_path_prints_url_and_outcome() {
        let jira = FakeJiraClient::new().with_issue("PROJ-372", issue("PROJ-372"));
        let gh = FakeGhCli::new().with_pr_view(Ok(Some(pr())));
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let mut out = Vec::new();

        run(&ctx, "proj-372", &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("https://example.atlassian.net/browse/PROJ-372"));
        assert!(output.contains("Title updated"));
        assert!(output.contains("Remote link added"));
    }

    #[test]
    fn invalid_key_format_is_an_actionable_error() {
        let jira = FakeJiraClient::new();
        let gh = FakeGhCli::new();
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let mut out = Vec::new();

        let err = run(&ctx, "not-a-key!", &mut out).expect_err("should fail");
        match err {
            TicketCliError::InvalidKey { key } => assert_eq!(key, "not-a-key!"),
            other => panic!("expected InvalidKey, got {other:?}"),
        }
    }

    #[test]
    fn not_found_error_message_is_passed_through() {
        let jira = FakeJiraClient::new().with_issue_not_found("PROJ-999");
        let gh = FakeGhCli::new().with_pr_view(Ok(Some(pr())));
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let mut out = Vec::new();

        let err = run(&ctx, "proj-999", &mut out).expect_err("should fail");
        match err {
            TicketCliError::Ticketing(TicketingError::Jira(JiraError::NotFound { key })) => {
                assert_eq!(key, "PROJ-999")
            }
            other => panic!("expected Jira NotFound, got {other:?}"),
        }
    }

    #[test]
    fn no_pr_for_branch_error_passes_through() {
        let jira = FakeJiraClient::new().with_issue("PROJ-372", issue("PROJ-372"));
        let gh = FakeGhCli::new()
            .with_pr_view(Ok(None))
            .with_current_branch(Ok("proj-372-fix".to_string()));
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let mut out = Vec::new();

        let err = run(&ctx, "PROJ-372", &mut out).expect_err("should fail");
        match err {
            TicketCliError::Ticketing(TicketingError::NoPrForBranch { branch }) => {
                assert_eq!(branch, "proj-372-fix")
            }
            other => panic!("expected NoPrForBranch, got {other:?}"),
        }
    }
}
