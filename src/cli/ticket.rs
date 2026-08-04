//! `tm ticket <KEY>`, `tm ticket create`, `tm ticket transition`, `tm
//! ticket assign`, and `tm ticket rank`.

use std::io::Write;

use regex::Regex;
use thiserror::Error;

use crate::config::Config;
use crate::jira::client::{JiraClient, RankAnchor};
use crate::ticketing::{
    AssignOutcome, AssignTarget, CreateTicketContext, TicketingContext, TicketingError,
    TransitionOutcome, assign_ticket, associate_ticket, create_ticket, list_transitions,
    rank_ticket, transition_ticket,
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

    /// `tm ticket rank <KEY> (--above|--below) <OTHER>` was given the same
    /// key (after normalization) for both `KEY` and `OTHER`. Rejected here
    /// rather than left to the Jira API, whose behavior ranking an issue
    /// relative to itself is undefined/unhelpful.
    #[error("cannot rank {key} relative to itself")]
    RankRelativeToSelf {
        /// The key given for both the ticket to rank and its anchor.
        key: String,
    },

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

/// `tm ticket transition <KEY> [STATUS]`: move `key` to `status`'s workflow
/// status, or, if `status` is omitted, list `key`'s current status and
/// available transitions.
///
/// Unlike `tm ticket create`/`tm pr create`'s advisory
/// `status_on_create`/`status_on_pr` transitions (which never fail the
/// overall command), this command is an explicit request to change status:
/// a mismatched status name or Jira API failure is a hard error (propagated
/// via [`TicketCliError::Ticketing`]), not a warning. Only the Jira client
/// is needed — this command has nothing to do with a pull request, `gh`, or
/// `git`.
pub fn transition(
    jira: &dyn JiraClient,
    key: &str,
    status: Option<&str>,
    out: &mut dyn Write,
) -> Result<(), TicketCliError> {
    let normalized = normalize_key(key)?;
    match status {
        Some(status) => transition_to_status(jira, &normalized, status, out),
        None => print_available_transitions(jira, &normalized, out),
    }
}

/// Apply `target` to `key` via [`transition_ticket`] and print the outcome.
fn transition_to_status(
    jira: &dyn JiraClient,
    key: &str,
    target: &str,
    out: &mut dyn Write,
) -> Result<(), TicketCliError> {
    match transition_ticket(jira, key, target)? {
        TransitionOutcome::Applied(resolved_status) => {
            writeln!(out, "Moved {key} to {resolved_status}")?;
        }
        TransitionOutcome::AlreadyInStatus(current_status) => {
            writeln!(out, "{key} is already in {current_status}")?;
        }
    }
    Ok(())
}

/// Print `key`'s current status and available transitions via
/// [`list_transitions`].
fn print_available_transitions(
    jira: &dyn JiraClient,
    key: &str,
    out: &mut dyn Write,
) -> Result<(), TicketCliError> {
    let listing = list_transitions(jira, key)?;
    writeln!(
        out,
        "{key} is in {}. Available transitions:",
        listing.current_status
    )?;
    if listing.transitions.is_empty() {
        // A ticket with no available transitions (e.g. a closed one) would
        // otherwise leave nothing but the header, which reads as broken
        // output rather than an empty-but-valid result.
        writeln!(out, "No transitions available.")?;
    }
    for t in &listing.transitions {
        writeln!(out, "{} -> {}", t.name, t.to.name)?;
    }
    Ok(())
}

/// `tm ticket assign <KEY> [NAME] [--me] [--unassign]`: assign `key` by
/// resolved name, to the current user, or clear its assignee.
///
/// Exactly one of `name`, `me`, `unassign` is expected to be set — clap's
/// `ArgGroup` on [`super::TicketCmd::Assign`] enforces this before this
/// function is ever called. Like [`transition`], every failure is a hard
/// error propagated via [`TicketCliError::Ticketing`]: an ambiguous or
/// unknown name, or any Jira API failure.
pub fn assign(
    jira: &dyn JiraClient,
    config: &Config,
    key: &str,
    name: Option<&str>,
    me: bool,
    unassign: bool,
    out: &mut dyn Write,
) -> Result<(), TicketCliError> {
    let normalized = normalize_key(key)?;
    let target = if unassign {
        AssignTarget::Unassign
    } else if me {
        AssignTarget::Me
    } else {
        AssignTarget::Name(
            name.expect("clap's ArgGroup guarantees one of name/me/unassign is set")
                .to_string(),
        )
    };

    match assign_ticket(jira, config, &normalized, &target)? {
        AssignOutcome::AssignedToUser(display_name) => {
            writeln!(out, "Assigned {normalized} to {display_name}")?;
        }
        AssignOutcome::AssignedToMe(label) => {
            writeln!(out, "Assigned {normalized} to me ({label})")?;
        }
        AssignOutcome::Unassigned => {
            writeln!(out, "Unassigned {normalized}")?;
        }
    }
    Ok(())
}

/// `tm ticket rank <KEY> (--above <OTHER> | --below <OTHER>)`: move `key`
/// above or below `other` in Jira's native backlog rank.
///
/// Exactly one of `above`/`below` is expected to be `Some` — clap's
/// `ArgGroup` on [`super::TicketCmd::Rank`] enforces this before this
/// function is ever called. Both keys are normalized via [`normalize_key`];
/// ranking `key` relative to itself is rejected as
/// [`TicketCliError::RankRelativeToSelf`] before any Jira call is made. Like
/// [`transition`] and [`assign`], every other failure is a hard error
/// propagated via [`TicketCliError::Ticketing`].
pub fn rank(
    jira: &dyn JiraClient,
    key: &str,
    above: Option<&str>,
    below: Option<&str>,
    out: &mut dyn Write,
) -> Result<(), TicketCliError> {
    let normalized = normalize_key(key)?;
    let (other, jira_anchor, verb) = match (above, below) {
        (Some(other), None) => {
            let other = normalize_key(other)?;
            (other.clone(), RankAnchor::Before(other), "above")
        }
        (None, Some(other)) => {
            let other = normalize_key(other)?;
            (other.clone(), RankAnchor::After(other), "below")
        }
        _ => unreachable!("clap's ArgGroup guarantees exactly one of above/below is set"),
    };

    if normalized == other {
        return Err(TicketCliError::RankRelativeToSelf { key: normalized });
    }

    rank_ticket(jira, &normalized, jira_anchor)?;
    writeln!(out, "Ranked {normalized} {verb} {other}")?;
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
    use crate::jira::types::{Issue, IssueFields, Status, StatusCategory, Transition};

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
                issue_links: vec![],
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

    fn transition_fixture(id: &str, name: &str, to_status: &str) -> Transition {
        Transition {
            id: id.to_string(),
            name: name.to_string(),
            to: Status {
                name: to_status.to_string(),
                status_category: StatusCategory {
                    key: "indeterminate".to_string(),
                },
            },
        }
    }

    #[test]
    fn transition_with_status_moves_ticket_and_prints_resolved_status() {
        let jira = FakeJiraClient::new()
            .with_issue("PROJ-372", issue("PROJ-372"))
            .with_transitions(
                "PROJ-372",
                vec![transition_fixture("21", "Send to review", "In Review")],
            );
        let mut out = Vec::new();

        transition(&jira, "proj-372", Some("in review"), &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(output, "Moved PROJ-372 to In Review\n");
        assert_eq!(
            jira.transition_calls(),
            vec![("PROJ-372".to_string(), "21".to_string())]
        );
    }

    #[test]
    fn transition_with_status_already_in_status_is_a_no_op_success() {
        let mut already_in_review = issue("PROJ-372");
        already_in_review.fields.status.name = "In Review".to_string();
        let jira = FakeJiraClient::new().with_issue("PROJ-372", already_in_review);
        let mut out = Vec::new();

        transition(&jira, "PROJ-372", Some("in review"), &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(output, "PROJ-372 is already in In Review\n");
        assert!(jira.transition_calls().is_empty());
    }

    #[test]
    fn transition_with_status_no_matching_transition_is_a_hard_error() {
        let jira = FakeJiraClient::new()
            .with_issue("PROJ-372", issue("PROJ-372"))
            .with_transitions(
                "PROJ-372",
                vec![transition_fixture("11", "Start Progress", "In Progress")],
            );
        let mut out = Vec::new();

        let err = transition(&jira, "PROJ-372", Some("In Review"), &mut out).expect_err(
            "should fail hard when no transition matches, unlike the advisory pr-create path",
        );

        match err {
            TicketCliError::Ticketing(TicketingError::NoMatchingTransition {
                key,
                target,
                available,
            }) => {
                assert_eq!(key, "PROJ-372");
                assert_eq!(target, "In Review");
                assert!(available.contains("Start Progress"));
                assert!(available.contains("In Progress"));
            }
            other => panic!("expected NoMatchingTransition, got {other:?}"),
        }
        assert!(out.is_empty(), "nothing should be printed on hard failure");
    }

    #[test]
    fn transition_with_status_api_error_is_a_hard_error() {
        let jira = FakeJiraClient::new().with_issue_not_found("PROJ-404");
        let mut out = Vec::new();

        let err = transition(&jira, "PROJ-404", Some("In Review"), &mut out)
            .expect_err("should fail hard");

        match err {
            TicketCliError::Ticketing(TicketingError::Jira(JiraError::NotFound { key })) => {
                assert_eq!(key, "PROJ-404")
            }
            other => panic!("expected Jira NotFound, got {other:?}"),
        }
    }

    #[test]
    fn transition_without_status_lists_current_status_and_available_transitions() {
        let jira = FakeJiraClient::new()
            .with_issue("PROJ-372", issue("PROJ-372"))
            .with_transitions(
                "PROJ-372",
                vec![transition_fixture("11", "Start Progress", "In Progress")],
            );
        let mut out = Vec::new();

        transition(&jira, "proj-372", None, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(
            output,
            "PROJ-372 is in To Do. Available transitions:\nStart Progress -> In Progress\n"
        );
    }

    #[test]
    fn transition_without_status_and_no_transitions_available_says_so() {
        let jira = FakeJiraClient::new()
            .with_issue("PROJ-372", issue("PROJ-372"))
            .with_transitions("PROJ-372", vec![]);
        let mut out = Vec::new();

        transition(&jira, "proj-372", None, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(
            output,
            "PROJ-372 is in To Do. Available transitions:\nNo transitions available.\n"
        );
    }

    fn jira_user(account_id: &str, display_name: &str) -> crate::jira::types::JiraUser {
        crate::jira::types::JiraUser {
            account_id: account_id.to_string(),
            display_name: display_name.to_string(),
        }
    }

    #[test]
    fn assign_by_name_prints_assigned_message() {
        let jira = FakeJiraClient::new()
            .with_assignable_users("PROJ", vec![jira_user("acct-1", "Jane Doe")]);
        let cfg = config();
        let mut out = Vec::new();

        assign(
            &jira,
            &cfg,
            "proj-372",
            Some("Jane"),
            false,
            false,
            &mut out,
        )
        .expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(output, "Assigned PROJ-372 to Jane Doe\n");
        assert_eq!(
            jira.assign_calls(),
            vec![("PROJ-372".to_string(), Some("acct-1".to_string()))]
        );
    }

    #[test]
    fn assign_by_name_ambiguous_is_a_hard_error() {
        let jira = FakeJiraClient::new().with_assignable_users(
            "PROJ",
            vec![
                jira_user("acct-1", "Jane Doe"),
                jira_user("acct-2", "Jane Smith"),
            ],
        );
        let cfg = config();
        let mut out = Vec::new();

        let err = assign(
            &jira,
            &cfg,
            "PROJ-372",
            Some("jane"),
            false,
            false,
            &mut out,
        )
        .expect_err("should fail");

        match err {
            TicketCliError::Ticketing(TicketingError::NoMatchingAssignee { key, name, .. }) => {
                assert_eq!(key, "PROJ-372");
                assert_eq!(name, "jane");
            }
            other => panic!("expected NoMatchingAssignee, got {other:?}"),
        }
        assert!(out.is_empty());
    }

    #[test]
    fn assign_me_uses_cached_account_id() {
        let jira = FakeJiraClient::new();
        let cfg = Config {
            default_assignee_account_id: Some("acct-1".to_string()),
            ..config()
        };
        let mut out = Vec::new();

        assign(&jira, &cfg, "PROJ-372", None, true, false, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(output, "Assigned PROJ-372 to me (acct-1)\n");
    }

    #[test]
    fn assign_me_falls_back_to_myself_display_name() {
        let jira = FakeJiraClient::new().with_myself(crate::jira::types::Myself {
            account_id: "acct-me".to_string(),
            display_name: "Ada Lovelace".to_string(),
            email_address: None,
        });
        let cfg = Config {
            default_assignee_account_id: None,
            ..config()
        };
        let mut out = Vec::new();

        assign(&jira, &cfg, "PROJ-372", None, true, false, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(output, "Assigned PROJ-372 to me (Ada Lovelace)\n");
    }

    #[test]
    fn assign_unassign_clears_assignee() {
        let jira = FakeJiraClient::new();
        let cfg = config();
        let mut out = Vec::new();

        assign(&jira, &cfg, "PROJ-372", None, false, true, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(output, "Unassigned PROJ-372\n");
        assert_eq!(jira.assign_calls(), vec![("PROJ-372".to_string(), None)]);
    }

    #[test]
    fn assign_invalid_key_is_an_actionable_error() {
        let jira = FakeJiraClient::new();
        let cfg = config();
        let mut out = Vec::new();

        let err = assign(&jira, &cfg, "not-a-key!", None, true, false, &mut out)
            .expect_err("should fail");
        match err {
            TicketCliError::InvalidKey { key } => assert_eq!(key, "not-a-key!"),
            other => panic!("expected InvalidKey, got {other:?}"),
        }
    }

    #[test]
    fn transition_without_status_invalid_key_is_an_actionable_error() {
        let jira = FakeJiraClient::new();
        let mut out = Vec::new();

        let err = transition(&jira, "not-a-key!", None, &mut out).expect_err("should fail");
        match err {
            TicketCliError::InvalidKey { key } => assert_eq!(key, "not-a-key!"),
            other => panic!("expected InvalidKey, got {other:?}"),
        }
    }

    #[test]
    fn rank_above_prints_ranked_message_and_calls_rank_with_before_anchor() {
        let jira = FakeJiraClient::new().with_issue("PROJ-372", issue("PROJ-372"));
        let mut out = Vec::new();

        rank(&jira, "proj-372", Some("proj-1"), None, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(output, "Ranked PROJ-372 above PROJ-1\n");
        assert_eq!(
            jira.rank_calls(),
            vec![(
                vec!["PROJ-372".to_string()],
                crate::jira::client::RankAnchor::Before("PROJ-1".to_string())
            )]
        );
    }

    #[test]
    fn rank_below_prints_ranked_message_and_calls_rank_with_after_anchor() {
        let jira = FakeJiraClient::new().with_issue("PROJ-372", issue("PROJ-372"));
        let mut out = Vec::new();

        rank(&jira, "proj-372", None, Some("proj-1"), &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(output, "Ranked PROJ-372 below PROJ-1\n");
        assert_eq!(
            jira.rank_calls(),
            vec![(
                vec!["PROJ-372".to_string()],
                crate::jira::client::RankAnchor::After("PROJ-1".to_string())
            )]
        );
    }

    #[test]
    fn rank_relative_to_self_is_a_usage_error() {
        let jira = FakeJiraClient::new().with_issue("PROJ-372", issue("PROJ-372"));
        let mut out = Vec::new();

        let err = rank(&jira, "proj-372", Some("PROJ-372"), None, &mut out).expect_err(
            "ranking a ticket relative to itself should fail before any Jira call is made",
        );

        match err {
            TicketCliError::RankRelativeToSelf { key } => assert_eq!(key, "PROJ-372"),
            other => panic!("expected RankRelativeToSelf, got {other:?}"),
        }
        assert!(jira.rank_calls().is_empty());
        assert!(out.is_empty());
    }

    #[test]
    fn rank_invalid_primary_key_is_an_actionable_error() {
        let jira = FakeJiraClient::new();
        let mut out = Vec::new();

        let err =
            rank(&jira, "not-a-key!", Some("PROJ-1"), None, &mut out).expect_err("should fail");
        match err {
            TicketCliError::InvalidKey { key } => assert_eq!(key, "not-a-key!"),
            other => panic!("expected InvalidKey, got {other:?}"),
        }
    }

    #[test]
    fn rank_invalid_anchor_key_is_an_actionable_error() {
        let jira = FakeJiraClient::new().with_issue("PROJ-372", issue("PROJ-372"));
        let mut out = Vec::new();

        let err =
            rank(&jira, "proj-372", Some("not-a-key!"), None, &mut out).expect_err("should fail");
        match err {
            TicketCliError::InvalidKey { key } => assert_eq!(key, "not-a-key!"),
            other => panic!("expected InvalidKey, got {other:?}"),
        }
    }

    #[test]
    fn rank_missing_primary_key_gives_friendly_not_found_error() {
        let jira = FakeJiraClient::new().with_issue_not_found("PROJ-404");
        let mut out = Vec::new();

        let err = rank(&jira, "proj-404", Some("PROJ-1"), None, &mut out).expect_err("should fail");

        match err {
            TicketCliError::Ticketing(TicketingError::Jira(JiraError::NotFound { key })) => {
                assert_eq!(key, "PROJ-404")
            }
            other => panic!("expected Jira NotFound, got {other:?}"),
        }
        assert!(jira.rank_calls().is_empty());
    }

    #[test]
    fn rank_anchor_error_surfaces_from_rank_call() {
        let jira = FakeJiraClient::new()
            .with_issue("PROJ-372", issue("PROJ-372"))
            .with_rank_error(500, "boom");
        let mut out = Vec::new();

        let err = rank(&jira, "proj-372", Some("PROJ-1"), None, &mut out).expect_err("should fail");

        match err {
            TicketCliError::Ticketing(TicketingError::Jira(JiraError::Api { status, message })) => {
                assert_eq!(status, 500);
                assert_eq!(message, "boom");
            }
            other => panic!("expected Jira Api error, got {other:?}"),
        }
    }
}
