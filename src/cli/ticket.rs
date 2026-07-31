//! `tm ticket <KEY>` and `tm ticket create`.

use std::io::Write;

use regex::Regex;
use thiserror::Error;

use crate::ticketing::{
    CreateTicketContext, TicketingContext, TicketingError, associate_ticket, create_ticket,
};

/// Errors surfaced by `tm ticket`.
#[derive(Debug, Error)]
pub enum TicketCliError {
    /// `key` didn't normalize to a valid Jira issue key shape.
    #[error("invalid ticket key `{key}`; expected a Jira key like PROJ-123")]
    InvalidKey {
        /// The key as originally passed on the command line.
        key: String,
    },

    /// Neither an issue key nor `create` was given.
    #[error("expected a Jira key (e.g. PROJ-123) or `tm ticket create`")]
    KeyOrCreateRequired,

    /// Neither `--title` nor an interactive prompt produced a non-empty
    /// title for `tm ticket create`.
    #[error("ticket title is required; pass --title or answer the prompt")]
    TitleRequired,

    /// Association with the current branch's pull request failed.
    #[error(transparent)]
    Ticketing(#[from] TicketingError),

    /// Writing output failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Options for `tm ticket create`, mirroring its CLI flags.
#[derive(Debug, Clone, Default)]
pub struct CreateOptions {
    /// Ticket title (Jira summary); prompted for interactively if `None`.
    pub title: Option<String>,
    /// Ticket description, as GitHub-flavored Markdown; no description if
    /// `None`.
    pub body: Option<String>,
}

/// `tm ticket create`: create a new ticket in the configured default
/// project, with no pull request involved.
pub fn create(
    ctx: &CreateTicketContext,
    opts: &CreateOptions,
    prompter: &mut dyn super::Prompter,
    out: &mut dyn Write,
) -> Result<(), TicketCliError> {
    let title = match &opts.title {
        Some(title) => title.clone(),
        None => prompter.prompt_line("Ticket title", "")?,
    };
    if title.trim().is_empty() {
        return Err(TicketCliError::TitleRequired);
    }

    let outcome = create_ticket(ctx, &title, opts.body.as_deref())?;
    writeln!(
        out,
        "Created ticket {}: {}",
        outcome.issue_key, outcome.issue_url
    )?;
    super::print_status_transition(&outcome.issue_key, &outcome.status_transition, out)?;

    Ok(())
}

/// `tm ticket <KEY>`: normalize and validate `key`, then associate it with
/// the pull request open for the current branch. Never transitions the
/// ticket's status; see [`crate::ticketing::associate_ticket`].
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
            status_on_create: None,
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

    fn create_ctx<'a>(jira: &'a FakeJiraClient, cfg: &'a Config) -> CreateTicketContext<'a> {
        CreateTicketContext { jira, config: cfg }
    }

    #[test]
    fn create_with_title_flag_creates_ticket() {
        let jira = FakeJiraClient::new().with_create_issue_result(issue("PROJ-9"));
        let cfg = config();
        let ctx = create_ctx(&jira, &cfg);
        let opts = CreateOptions {
            title: Some("Add the widget".to_string()),
            body: None,
        };
        let mut prompter = crate::cli::FakePrompter::new();
        let mut out = Vec::new();

        create(&ctx, &opts, &mut prompter, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(
            output.contains("Created ticket PROJ-9: https://example.atlassian.net/browse/PROJ-9")
        );
        assert_eq!(jira.create_issue_calls().len(), 1);
        assert_eq!(jira.create_issue_calls()[0].summary, "Add the widget");
        assert!(
            prompter.messages.is_empty(),
            "should not prompt when --title is given"
        );
    }

    #[test]
    fn create_prompts_for_missing_title() {
        let jira = FakeJiraClient::new().with_create_issue_result(issue("PROJ-9"));
        let cfg = config();
        let ctx = create_ctx(&jira, &cfg);
        let opts = CreateOptions::default();
        let mut prompter = crate::cli::FakePrompter::new().with_line("Add the widget");
        let mut out = Vec::new();

        create(&ctx, &opts, &mut prompter, &mut out).expect("should succeed");

        assert_eq!(jira.create_issue_calls()[0].summary, "Add the widget");
        assert_eq!(prompter.messages, vec!["Ticket title".to_string()]);
    }

    #[test]
    fn create_missing_title_prompts_and_fails_if_still_empty() {
        let jira = FakeJiraClient::new();
        let cfg = config();
        let ctx = create_ctx(&jira, &cfg);
        let opts = CreateOptions::default();
        let mut prompter = crate::cli::FakePrompter::new().with_line("");
        let mut out = Vec::new();

        let err = create(&ctx, &opts, &mut prompter, &mut out).expect_err("should fail");
        assert!(matches!(err, TicketCliError::TitleRequired));
        assert!(jira.create_issue_calls().is_empty());
    }

    #[test]
    fn create_with_body_converts_markdown_to_adf_description() {
        let jira = FakeJiraClient::new().with_create_issue_result(issue("PROJ-9"));
        let cfg = config();
        let ctx = create_ctx(&jira, &cfg);
        let opts = CreateOptions {
            title: Some("Add the widget".to_string()),
            body: Some("**bold** details".to_string()),
        };
        let mut prompter = crate::cli::FakePrompter::new();
        let mut out = Vec::new();

        create(&ctx, &opts, &mut prompter, &mut out).expect("should succeed");

        let description = jira.create_issue_calls()[0].description.to_string();
        assert!(
            description.contains("\"strong\""),
            "markdown body should be converted to ADF marks: {description}"
        );
    }

    #[test]
    fn create_without_body_has_empty_description() {
        let jira = FakeJiraClient::new().with_create_issue_result(issue("PROJ-9"));
        let cfg = config();
        let ctx = create_ctx(&jira, &cfg);
        let opts = CreateOptions {
            title: Some("Add the widget".to_string()),
            body: None,
        };
        let mut prompter = crate::cli::FakePrompter::new();
        let mut out = Vec::new();

        create(&ctx, &opts, &mut prompter, &mut out).expect("should succeed");

        assert_eq!(
            jira.create_issue_calls()[0].description,
            serde_json::json!({ "type": "doc", "version": 1, "content": [] })
        );
    }

    #[test]
    fn create_prints_moved_line_when_status_on_create_transition_applies() {
        use crate::jira::types::Transition;

        let jira = FakeJiraClient::new()
            .with_create_issue_result(issue("PROJ-9"))
            .with_transitions(
                "PROJ-9",
                vec![Transition {
                    id: "11".to_string(),
                    name: "Start Progress".to_string(),
                    to: Status {
                        name: "In Progress".to_string(),
                        status_category: StatusCategory {
                            key: "indeterminate".to_string(),
                        },
                    },
                }],
            );
        let cfg = Config {
            status_on_create: Some("In Progress".to_string()),
            ..config()
        };
        let ctx = create_ctx(&jira, &cfg);
        let opts = CreateOptions {
            title: Some("Add the widget".to_string()),
            body: None,
        };
        let mut prompter = crate::cli::FakePrompter::new();
        let mut out = Vec::new();

        create(&ctx, &opts, &mut prompter, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("Moved PROJ-9 to In Progress"));
    }

    #[test]
    fn create_prints_warning_line_when_no_matching_transition() {
        let jira = FakeJiraClient::new().with_create_issue_result(issue("PROJ-9"));
        let cfg = Config {
            status_on_create: Some("In Progress".to_string()),
            ..config()
        };
        let ctx = create_ctx(&jira, &cfg);
        let opts = CreateOptions {
            title: Some("Add the widget".to_string()),
            body: None,
        };
        let mut prompter = crate::cli::FakePrompter::new();
        let mut out = Vec::new();

        create(&ctx, &opts, &mut prompter, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("warning:"));
    }

    #[test]
    fn create_prints_nothing_extra_when_status_on_create_unset() {
        let jira = FakeJiraClient::new().with_create_issue_result(issue("PROJ-9"));
        let cfg = config();
        let ctx = create_ctx(&jira, &cfg);
        let opts = CreateOptions {
            title: Some("Add the widget".to_string()),
            body: None,
        };
        let mut prompter = crate::cli::FakePrompter::new();
        let mut out = Vec::new();

        create(&ctx, &opts, &mut prompter, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(!output.contains("Moved"));
        assert!(!output.contains("warning:"));
    }
}
