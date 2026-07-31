//! `tm pr create` and `tm pr status`.

use std::io::Write;

use thiserror::Error;

use crate::github::gh_cli::{GhError, PrCreateRequest};
use crate::ticketing::{
    TicketingContext, TicketingError, associate_ticket, auto_create_and_associate,
    resolve_existing_key,
};

/// Errors surfaced by `tm pr create` and `tm pr status`.
#[derive(Debug, Error)]
pub enum PrCliError {
    /// Neither `--title` nor an interactive prompt produced a non-empty
    /// title.
    #[error("PR title is required; pass --title or answer the prompt")]
    TitleRequired,

    /// The current branch has no open pull request.
    #[error("no pull request found for branch `{branch}`. Run `tm pr create` first.")]
    NoPr {
        /// The branch that has no open pull request.
        branch: String,
    },

    /// A `gh`/`git` shell-out failed.
    #[error(transparent)]
    Gh(#[from] GhError),

    /// Ticket association or creation failed.
    #[error(transparent)]
    Ticketing(#[from] TicketingError),

    /// A prompt or output write failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Options for `tm pr create`, mirroring its CLI flags.
#[derive(Debug, Clone, Default)]
pub struct PrCreateOptions {
    /// Pull request title; prompted for interactively if `None`.
    pub title: Option<String>,
    /// Pull request body; empty if `None`.
    pub body: Option<String>,
    /// Base branch to open the PR against.
    pub base: Option<String>,
}

/// Options for `tm pr status`, mirroring its CLI flags.
#[derive(Debug, Clone, Copy, Default)]
pub struct PrStatusOptions {
    /// Auto-create a ticket if none is associated, without prompting.
    pub auto_ticket: bool,
}

/// `tm pr create`: open a pull request for the current branch, then
/// associate a ticket with it (an existing key if the title/body already
/// carries one, otherwise a freshly created one).
pub fn create(
    ctx: &TicketingContext,
    opts: &PrCreateOptions,
    prompter: &mut dyn super::Prompter,
    out: &mut dyn Write,
) -> Result<(), PrCliError> {
    let title = match &opts.title {
        Some(title) => title.clone(),
        None => prompter.prompt_line("PR title", "")?,
    };
    if title.trim().is_empty() {
        return Err(PrCliError::TitleRequired);
    }

    let req = PrCreateRequest {
        title,
        body: opts.body.clone().unwrap_or_default(),
        base: opts.base.clone(),
    };
    let pr = ctx.gh.pr_create(&req)?;
    writeln!(out, "Created PR #{}: {}", pr.number, pr.url)?;

    let outcome = match resolve_existing_key(ctx.jira, &pr)? {
        Some(key) => associate_ticket(ctx, &key)?,
        None => auto_create_and_associate(ctx, &pr)?,
    };
    writeln!(out, "Ticket {}: {}", outcome.issue_key, outcome.issue_url)?;

    Ok(())
}

/// `tm pr status`: report the pull request open for the current branch and
/// which ticket (if any) is associated with it.
pub fn status(
    ctx: &TicketingContext,
    opts: &PrStatusOptions,
    prompter: &mut dyn super::Prompter,
    out: &mut dyn Write,
) -> Result<(), PrCliError> {
    let pr = match ctx.gh.pr_view()? {
        Some(pr) => pr,
        None => {
            let branch = ctx.gh.current_branch()?;
            writeln!(
                out,
                "no pull request found for branch `{branch}`. Run `tm pr create` first."
            )?;
            return Err(PrCliError::NoPr { branch });
        }
    };

    writeln!(out, "PR #{}: {}", pr.number, pr.url)?;
    writeln!(out, "Title: {}", pr.title)?;

    match resolve_existing_key(ctx.jira, &pr)? {
        Some(key) => {
            let issue_url = format!("{}/browse/{key}", ctx.config.jira_base_url);
            writeln!(out, "Ticket {key}: {issue_url}")?;
        }
        None => {
            writeln!(out, "no ticket associated")?;
            let should_create = if opts.auto_ticket {
                true
            } else {
                prompter.confirm(&format!(
                    "Create a ticket in {}?",
                    ctx.config.default_project_key
                ))?
            };
            if should_create {
                let outcome = auto_create_and_associate(ctx, &pr)?;
                writeln!(
                    out,
                    "Created ticket {}: {}",
                    outcome.issue_key, outcome.issue_url
                )?;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::FakePrompter;
    use crate::config::Config;
    use crate::github::gh_cli::FakeGhCli;
    use crate::github::pr::PrInfo;
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

    fn pr_with_title(title: &str) -> PrInfo {
        PrInfo {
            number: 42,
            url: "https://github.com/example/repo/pull/42".to_string(),
            title: title.to_string(),
            body: String::new(),
            head_ref_name: "some-branch".to_string(),
        }
    }

    fn config() -> Config {
        Config {
            jira_base_url: "https://example.atlassian.net".to_string(),
            jira_email: "dev@example.com".to_string(),
            default_project_key: "PROJ".to_string(),
            default_assignee_account_id: Some("acct-1".to_string()),
        }
    }

    #[test]
    fn create_with_flag_title_auto_creates_ticket_when_none_associated() {
        let jira = FakeJiraClient::new().with_create_issue_result(issue("PROJ-9"));
        let gh = FakeGhCli::new().with_pr_create_result(Ok(pr_with_title("Add the widget")));
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let opts = PrCreateOptions {
            title: Some("Add the widget".to_string()),
            body: Some("Details".to_string()),
            base: None,
        };
        let mut prompter = FakePrompter::new();
        let mut out = Vec::new();

        create(&ctx, &opts, &mut prompter, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("Created PR #42: https://github.com/example/repo/pull/42"));
        assert!(output.contains("Ticket PROJ-9: https://example.atlassian.net/browse/PROJ-9"));
        assert_eq!(jira.create_issue_calls().len(), 1);
    }

    #[test]
    fn create_missing_title_prompts_and_fails_if_still_empty() {
        let jira = FakeJiraClient::new();
        let gh = FakeGhCli::new();
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let opts = PrCreateOptions::default();
        let mut prompter = FakePrompter::new().with_line("");
        let mut out = Vec::new();

        let err = create(&ctx, &opts, &mut prompter, &mut out).expect_err("should fail");
        assert!(matches!(err, PrCliError::TitleRequired));
    }

    #[test]
    fn status_with_no_pr_reports_message_and_fails() {
        let jira = FakeJiraClient::new();
        let gh = FakeGhCli::new()
            .with_pr_view(Ok(None))
            .with_current_branch(Ok("some-branch".to_string()));
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let opts = PrStatusOptions::default();
        let mut prompter = FakePrompter::new();
        let mut out = Vec::new();

        let err = status(&ctx, &opts, &mut prompter, &mut out).expect_err("should fail");
        match err {
            PrCliError::NoPr { branch } => assert_eq!(branch, "some-branch"),
            other => panic!("expected NoPr, got {other:?}"),
        }
        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("no pull request found for branch `some-branch`"));
    }

    #[test]
    fn status_with_existing_key_does_not_call_jira_create() {
        let jira = FakeJiraClient::new();
        let gh = FakeGhCli::new().with_pr_view(Ok(Some(pr_with_title("[PROJ-372] Fix the thing"))));
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let opts = PrStatusOptions::default();
        let mut prompter = FakePrompter::new();
        let mut out = Vec::new();

        status(&ctx, &opts, &mut prompter, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("Ticket PROJ-372: https://example.atlassian.net/browse/PROJ-372"));
        assert_eq!(jira.create_issue_calls().len(), 0);
    }

    #[test]
    fn status_with_auto_ticket_flag_creates_ticket_without_prompting() {
        let jira = FakeJiraClient::new().with_create_issue_result(issue("PROJ-9"));
        let gh = FakeGhCli::new().with_pr_view(Ok(Some(pr_with_title("Fix the thing"))));
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let opts = PrStatusOptions { auto_ticket: true };
        let mut prompter = FakePrompter::new();
        let mut out = Vec::new();

        status(&ctx, &opts, &mut prompter, &mut out).expect("should succeed");

        assert_eq!(jira.create_issue_calls().len(), 1);
        assert!(
            prompter.messages.is_empty(),
            "should not prompt when --auto-ticket is set"
        );
        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("Created ticket PROJ-9"));
    }

    #[test]
    fn status_prompt_declined_does_not_create_ticket() {
        let jira = FakeJiraClient::new();
        let gh = FakeGhCli::new().with_pr_view(Ok(Some(pr_with_title("Fix the thing"))));
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let opts = PrStatusOptions::default();
        let mut prompter = FakePrompter::new().with_confirm(false);
        let mut out = Vec::new();

        status(&ctx, &opts, &mut prompter, &mut out).expect("should succeed");

        assert_eq!(jira.create_issue_calls().len(), 0);
        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("no ticket associated"));
        assert!(!output.contains("Created ticket"));
    }
}
