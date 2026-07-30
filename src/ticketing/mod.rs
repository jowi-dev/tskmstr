//! Orchestration of ticket <-> pull request association.
//!
//! This module ties together [`crate::jira`] and [`crate::github`]: given a
//! Jira issue key and the pull request open for the current branch, it makes
//! the PR title carry the key and posts a Jira remote link pointing at the
//! PR. It does not itself talk to the network; all I/O goes through the
//! [`JiraClient`] and [`GhCli`] trait objects on [`TicketingContext`].

use thiserror::Error;

use crate::config::Config;
use crate::github::gh_cli::{GhCli, GhError, PrEditRequest};
use crate::github::pr::{KeySource, PrInfo, find_issue_key_with_source, with_issue_key_prefix};
use crate::jira::adf::text_to_adf;
use crate::jira::client::{JiraClient, JiraError};
use crate::jira::types::{CreateIssueRequest, RemoteLinkRequest};

/// Dependencies shared by the ticketing orchestration functions.
pub struct TicketingContext<'a> {
    /// Jira client used to verify issues and post remote links.
    pub jira: &'a dyn JiraClient,
    /// `gh` CLI wrapper used to look up and edit the current branch's PR.
    pub gh: &'a dyn GhCli,
    /// Resolved configuration (Jira base URL, default project, etc).
    pub config: &'a Config,
}

/// Result of successfully associating a Jira issue with a pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssociateOutcome {
    /// The associated issue's key.
    pub issue_key: String,
    /// Browsable URL of the associated issue.
    pub issue_url: String,
    /// Whether the PR title was changed to carry the issue key prefix.
    ///
    /// `false` when the title already carried the correct prefix, so no
    /// `gh pr edit` call was made.
    pub title_updated: bool,
    /// Whether a Jira remote link was posted for the PR.
    pub remote_link_added: bool,
}

/// Errors that can occur while associating a ticket with a pull request.
#[derive(Debug, Error)]
pub enum TicketingError {
    /// A Jira API call failed.
    #[error(transparent)]
    Jira(#[from] JiraError),

    /// A `gh`/`git` shell-out failed.
    #[error(transparent)]
    Gh(#[from] GhError),

    /// The current branch has no open pull request to associate a ticket
    /// with.
    #[error("no pull request found for branch `{branch}`. Run `tm pr create` first.")]
    NoPrForBranch {
        /// The branch that has no open pull request.
        branch: String,
    },
}

/// `tm ticket <KEY>`: verify `key` exists in Jira, then associate it with the
/// pull request open for the current branch.
///
/// Fails with the underlying [`JiraError`] (e.g. [`JiraError::NotFound`]) if
/// `key` does not exist, and with [`TicketingError::NoPrForBranch`] if the
/// current branch has no open pull request.
pub fn associate_ticket(
    ctx: &TicketingContext,
    key: &str,
) -> Result<AssociateOutcome, TicketingError> {
    ctx.jira.get_issue(key)?;
    let pr = current_branch_pr(ctx)?;
    associate(ctx, key, &pr)
}

/// Fetch the pull request for the current branch, turning "no PR" into
/// [`TicketingError::NoPrForBranch`] (which requires knowing the branch
/// name, hence the extra `current_branch` call on that path).
fn current_branch_pr(ctx: &TicketingContext) -> Result<PrInfo, TicketingError> {
    match ctx.gh.pr_view()? {
        Some(pr) => Ok(pr),
        None => {
            let branch = ctx.gh.current_branch()?;
            Err(TicketingError::NoPrForBranch { branch })
        }
    }
}

/// Create a new issue in the configured default project, assigned to the
/// configured default assignee, then associate it with `pr`.
///
/// Used when [`resolve_existing_key`] finds no key already associated with
/// `pr`. The new issue's summary is `pr.title` as-is (a PR reaching this
/// point has no key anywhere in title/body/branch, so there is no prefix to
/// strip); its description is `pr.body` followed by the PR URL, converted to
/// ADF.
pub fn auto_create_and_associate(
    ctx: &TicketingContext,
    pr: &PrInfo,
) -> Result<AssociateOutcome, TicketingError> {
    let description = text_to_adf(&format!("{}\n\n{}", pr.body, pr.url));
    let req = CreateIssueRequest {
        project_key: ctx.config.default_project_key.clone(),
        summary: pr.title.clone(),
        description,
        issue_type_name: "Task".to_string(),
        assignee_account_id: ctx.config.default_assignee_account_id.clone(),
    };
    let issue = ctx.jira.create_issue(&req)?;
    associate(ctx, &issue.key, pr)
}

/// Resolve an issue key already associated with `pr`, if any.
///
/// Delegates to [`find_issue_key_with_source`] for the title/body/branch
/// precedence, then treats the result differently depending on where it came
/// from:
///
/// - [`KeySource::Title`] and [`KeySource::Body`] keys are trusted without
///   contacting Jira: the user (or a prior `tm ticket`/`tm pr create` run)
///   wrote them deliberately.
/// - A [`KeySource::Branch`] key is inferred from a naming convention, not
///   authored, so it is validated with [`JiraClient::get_issue`] first.
///   [`JiraError::NotFound`] is treated as "no key after all" (`Ok(None)`);
///   any other Jira error propagates, since it means the check itself
///   couldn't be completed.
///
/// Returns `Ok(None)` when no key is found by any means.
pub fn resolve_existing_key(
    jira: &dyn JiraClient,
    pr: &PrInfo,
) -> Result<Option<String>, TicketingError> {
    match find_issue_key_with_source(pr) {
        Some((key, KeySource::Title | KeySource::Body)) => Ok(Some(key)),
        Some((key, KeySource::Branch)) => match jira.get_issue(&key) {
            Ok(_) => Ok(Some(key)),
            Err(JiraError::NotFound { .. }) => Ok(None),
            Err(other) => Err(other.into()),
        },
        None => Ok(None),
    }
}

/// Associate `key` with `pr`: idempotently prefix the PR title, then post a
/// Jira remote link pointing at the PR. Shared by every public entry point
/// that ends in an association.
fn associate(
    ctx: &TicketingContext,
    key: &str,
    pr: &PrInfo,
) -> Result<AssociateOutcome, TicketingError> {
    let prefixed_title = with_issue_key_prefix(&pr.title, key);
    let title_updated = prefixed_title != pr.title;
    if title_updated {
        ctx.gh.pr_edit(
            pr.number,
            &PrEditRequest {
                title: Some(prefixed_title),
                body: None,
            },
        )?;
    }

    let link = RemoteLinkRequest {
        url: pr.url.clone(),
        title: format!("GitHub PR #{}: {}", pr.number, pr.title),
    };
    ctx.jira.add_remote_link(key, &link)?;

    Ok(AssociateOutcome {
        issue_key: key.to_string(),
        issue_url: format!("{}/browse/{}", ctx.config.jira_base_url, key),
        title_updated,
        remote_link_added: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::gh_cli::FakeGhCli;
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

    fn pr(title: &str) -> PrInfo {
        PrInfo {
            number: 42,
            url: "https://github.com/example/repo/pull/42".to_string(),
            title: title.to_string(),
            body: String::new(),
            head_ref_name: "ax-372-fix".to_string(),
        }
    }

    fn config() -> Config {
        Config {
            jira_base_url: "https://example.atlassian.net".to_string(),
            jira_email: "ada@example.com".to_string(),
            default_project_key: "PROJ".to_string(),
            default_assignee_account_id: Some("acct-1".to_string()),
        }
    }

    #[test]
    fn associate_ticket_happy_path_prefixes_title_and_posts_remote_link() {
        let jira = FakeJiraClient::new().with_issue("PROJ-1", issue("PROJ-1"));
        let gh = FakeGhCli::new().with_pr_view(Ok(Some(pr("Fix the thing"))));
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };

        let outcome = associate_ticket(&ctx, "PROJ-1").expect("should succeed");

        assert_eq!(outcome.issue_key, "PROJ-1");
        assert_eq!(
            outcome.issue_url,
            "https://example.atlassian.net/browse/PROJ-1"
        );
        assert!(outcome.title_updated);
        assert!(outcome.remote_link_added);

        assert_eq!(
            gh.pr_edit_calls(),
            vec![(
                42,
                PrEditRequest {
                    title: Some("[PROJ-1] Fix the thing".to_string()),
                    body: None,
                }
            )]
        );
        assert_eq!(
            jira.add_remote_link_calls(),
            vec![(
                "PROJ-1".to_string(),
                RemoteLinkRequest {
                    url: "https://github.com/example/repo/pull/42".to_string(),
                    title: "GitHub PR #42: Fix the thing".to_string(),
                }
            )]
        );
    }

    #[test]
    fn associate_ticket_missing_issue_errors_with_key() {
        let jira = FakeJiraClient::new().with_issue_not_found("PROJ-404");
        let gh = FakeGhCli::new().with_pr_view(Ok(Some(pr("Fix the thing"))));
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };

        let err = associate_ticket(&ctx, "PROJ-404").expect_err("should fail");

        match err {
            TicketingError::Jira(JiraError::NotFound { key }) => assert_eq!(key, "PROJ-404"),
            other => panic!("expected Jira NotFound, got {other:?}"),
        }
        assert!(gh.pr_edit_calls().is_empty());
    }

    #[test]
    fn associate_ticket_no_pr_for_branch_errors_with_branch_name() {
        let jira = FakeJiraClient::new().with_issue("PROJ-1", issue("PROJ-1"));
        let gh = FakeGhCli::new()
            .with_pr_view(Ok(None))
            .with_current_branch(Ok("ax-372-fix".to_string()));
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };

        let err = associate_ticket(&ctx, "PROJ-1").expect_err("should fail");

        match err {
            TicketingError::NoPrForBranch { branch } => assert_eq!(branch, "ax-372-fix"),
            other => panic!("expected NoPrForBranch, got {other:?}"),
        }
        assert_eq!(jira.add_remote_link_calls().len(), 0);
    }

    #[test]
    fn associate_ticket_is_idempotent_when_title_already_prefixed() {
        let jira = FakeJiraClient::new().with_issue("PROJ-1", issue("PROJ-1"));
        let gh = FakeGhCli::new().with_pr_view(Ok(Some(pr("[PROJ-1] Fix the thing"))));
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };

        let outcome = associate_ticket(&ctx, "PROJ-1").expect("should succeed");

        assert!(!outcome.title_updated);
        assert!(outcome.remote_link_added);
        assert!(
            gh.pr_edit_calls().is_empty(),
            "no pr_edit call should be made when the title is already prefixed"
        );
        assert_eq!(jira.add_remote_link_calls().len(), 1);
    }

    #[test]
    fn auto_create_and_associate_creates_issue_with_expected_fields_and_associates() {
        let jira = FakeJiraClient::new().with_create_issue_result(issue("PROJ-9"));
        let gh = FakeGhCli::new();
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let mut pull_request = pr("Add the widget");
        pull_request.body = "Implements the widget end to end.".to_string();

        let outcome = auto_create_and_associate(&ctx, &pull_request).expect("should succeed");

        let calls = jira.create_issue_calls();
        assert_eq!(calls.len(), 1);
        let create_req = &calls[0];
        assert_eq!(create_req.project_key, "PROJ");
        assert_eq!(create_req.summary, "Add the widget");
        assert_eq!(create_req.issue_type_name, "Task");
        assert_eq!(create_req.assignee_account_id, Some("acct-1".to_string()));
        let description = create_req.description.to_string();
        assert!(
            description.contains("https://github.com/example/repo/pull/42"),
            "description should contain the PR URL: {description}"
        );
        assert!(
            description.contains("Implements the widget end to end."),
            "description should contain the PR body: {description}"
        );

        assert_eq!(outcome.issue_key, "PROJ-9");
        assert_eq!(
            gh.pr_edit_calls(),
            vec![(
                42,
                PrEditRequest {
                    title: Some("[PROJ-9] Add the widget".to_string()),
                    body: None,
                }
            )]
        );
        assert_eq!(jira.add_remote_link_calls().len(), 1);
    }

    #[test]
    fn resolve_existing_key_trusts_title_key_without_calling_jira() {
        let jira = FakeJiraClient::new();
        let pull_request = pr("[PROJ-1] Fix the thing");

        let key = resolve_existing_key(&jira, &pull_request).expect("should succeed");

        assert_eq!(key, Some("PROJ-1".to_string()));
        // No issue was seeded, so any get_issue call would have failed with
        // NotFound; the Ok result proves get_issue was never called.
    }

    #[test]
    fn resolve_existing_key_validates_branch_key_that_exists() {
        let jira = FakeJiraClient::new().with_issue("AX-372", issue("AX-372"));
        let pull_request = pr("Fix the thing");

        let key = resolve_existing_key(&jira, &pull_request).expect("should succeed");

        assert_eq!(key, Some("AX-372".to_string()));
    }

    #[test]
    fn resolve_existing_key_branch_key_not_found_is_none() {
        let jira = FakeJiraClient::new().with_issue_not_found("AX-372");
        let pull_request = pr("Fix the thing");

        let key = resolve_existing_key(&jira, &pull_request).expect("should succeed");

        assert_eq!(key, None);
    }

    #[test]
    fn resolve_existing_key_branch_key_other_error_propagates() {
        let jira = FakeJiraClient::new().with_issue_error("AX-372", 500, "boom");
        let pull_request = pr("Fix the thing");

        let err = resolve_existing_key(&jira, &pull_request).expect_err("should fail");

        match err {
            TicketingError::Jira(JiraError::Api { status, .. }) => assert_eq!(status, 500),
            other => panic!("expected Jira Api error, got {other:?}"),
        }
    }
}
