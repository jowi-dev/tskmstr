//! Command-line interface: argument parsing and thin per-command
//! orchestration.
//!
//! Each submodule (`auth`, `ticket`, `pr`) exposes functions that take
//! trait-object dependencies (a [`crate::jira::client::JiraClient`], a
//! [`crate::github::gh_cli::GhCli`], a [`Prompter`], an output sink) so they
//! can be exercised in tests with fakes, without touching a network, a
//! keychain, or a real terminal. `src/main.rs` is the only place that wires
//! up the real implementations.

use std::io::{self, Write};

use clap::{Parser, Subcommand};

pub mod auth;
pub mod pr;
pub mod ticket;

use crate::ticketing::StatusTransition;

/// Print the result of a ticket's `status_on_pr`/`status_on_create`
/// transition attempt, if one was made.
///
/// Prints nothing when `status_transition` is `None` (the relevant config
/// key isn't set, the ticket was already in the target status, or the
/// outcome came from a path that never transitions, like `tm ticket <KEY>`).
/// Shared by `cli::pr` (auto-created and pre-existing tickets) and
/// `cli::ticket` (`tm ticket create`) so the same "Moved ... to ..." /
/// "warning: ..." wording appears on every path.
pub(crate) fn print_status_transition(
    issue_key: &str,
    status_transition: &Option<StatusTransition>,
    out: &mut dyn Write,
) -> io::Result<()> {
    match status_transition {
        Some(StatusTransition::Applied(status)) => writeln!(out, "Moved {issue_key} to {status}"),
        Some(StatusTransition::Warning(message)) => writeln!(out, "warning: {message}"),
        None => Ok(()),
    }
}

/// `tm`: Jira tickets and GitHub PRs from the terminal.
#[derive(Parser, Debug)]
#[command(name = "tm", about = "Jira tickets and GitHub PRs from the terminal")]
pub struct Cli {
    /// Subcommand to run. When absent, `tm` opens the interactive board.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Top-level subcommands.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Manage Jira authentication.
    Auth {
        /// Which auth action to perform.
        #[command(subcommand)]
        cmd: AuthCmd,
    },
    /// Associate a Jira ticket with the pull request open for the current
    /// branch, or create a new one with `tm ticket create`.
    #[command(args_conflicts_with_subcommands = true)]
    Ticket {
        /// Jira issue key, e.g. `PROJ-372` (case-insensitive). Omit when
        /// using `tm ticket create`.
        key: Option<String>,
        /// Which ticket action to perform.
        #[command(subcommand)]
        cmd: Option<TicketCmd>,
    },
    /// Manage the pull request open for the current branch.
    Pr {
        /// Which PR action to perform.
        #[command(subcommand)]
        cmd: PrCmd,
    },
    /// Open the interactive terminal board.
    Board,
}

/// `tm ticket` subcommands.
#[derive(Subcommand, Debug)]
pub enum TicketCmd {
    /// Create a new ticket in the configured default project, independent
    /// of any pull request.
    Create {
        /// Ticket title (Jira summary). Prompted for interactively if
        /// omitted.
        #[arg(long)]
        title: Option<String>,
        /// Ticket description. Supports GitHub-flavored Markdown; omitted
        /// entirely if not given.
        #[arg(long)]
        body: Option<String>,
    },
}

/// `tm auth` subcommands.
#[derive(Subcommand, Debug)]
pub enum AuthCmd {
    /// Bootstrap config if needed, validate a Jira API token, and store it.
    Login,
    /// Report whether Jira auth is configured and working.
    Status,
}

/// `tm pr` subcommands.
#[derive(Subcommand, Debug)]
pub enum PrCmd {
    /// Open a pull request for the current branch and associate a ticket.
    Create {
        /// Pull request title. Prompted for interactively if omitted.
        #[arg(long)]
        title: Option<String>,
        /// Pull request body.
        #[arg(long)]
        body: Option<String>,
        /// Base branch to open the PR against.
        #[arg(long)]
        base: Option<String>,
    },
    /// Report the pull request open for the current branch and its ticket.
    Status {
        /// Automatically create a ticket if none is associated yet, without
        /// prompting.
        #[arg(long)]
        auto_ticket: bool,
    },
}

/// Interactive input the CLI needs beyond plain positional/flag arguments:
/// bootstrap prompts, the Jira API token, and yes/no confirmations.
///
/// A trait (rather than concrete stdin/stdout calls) so command logic can be
/// tested with a canned `FakePrompter` instead of driving a real terminal.
pub trait Prompter {
    /// Prompt for a line of text, showing `default` and returning it verbatim
    /// if the user answers with an empty line.
    fn prompt_line(&mut self, message: &str, default: &str) -> io::Result<String>;

    /// Prompt for a secret (e.g. an API token) without echoing input.
    fn prompt_password(&mut self, message: &str) -> io::Result<String>;

    /// Prompt for a yes/no confirmation, defaulting to "no" on an empty
    /// answer.
    fn confirm(&mut self, message: &str) -> io::Result<bool>;
}

/// [`Prompter`] backed by the real terminal (stdin for text, `rpassword` for
/// the token so it isn't echoed).
pub struct RealPrompter;

impl Prompter for RealPrompter {
    fn prompt_line(&mut self, message: &str, default: &str) -> io::Result<String> {
        if default.is_empty() {
            print!("{message}: ");
        } else {
            print!("{message} [{default}]: ");
        }
        io::stdout().flush()?;

        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        let trimmed = line.trim();
        Ok(if trimmed.is_empty() {
            default.to_string()
        } else {
            trimmed.to_string()
        })
    }

    fn prompt_password(&mut self, message: &str) -> io::Result<String> {
        rpassword::prompt_password(message)
    }

    fn confirm(&mut self, message: &str) -> io::Result<bool> {
        print!("{message} [y/N]: ");
        io::stdout().flush()?;

        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        Ok(matches!(line.trim(), "y" | "Y" | "yes" | "Yes" | "YES"))
    }
}

/// [`Prompter`] test double: returns canned answers from fixed queues and
/// records every prompt message it was asked, in order.
#[cfg(test)]
pub struct FakePrompter {
    lines: std::collections::VecDeque<String>,
    passwords: std::collections::VecDeque<String>,
    confirms: std::collections::VecDeque<bool>,
    /// Every message passed to any prompt method, in call order.
    pub messages: Vec<String>,
}

#[cfg(test)]
impl Default for FakePrompter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl FakePrompter {
    /// A prompter with no queued answers; any prompt call will panic. Use the
    /// `with_*` builders to queue answers a test expects to be consumed.
    pub fn new() -> Self {
        Self {
            lines: std::collections::VecDeque::new(),
            passwords: std::collections::VecDeque::new(),
            confirms: std::collections::VecDeque::new(),
            messages: Vec::new(),
        }
    }

    /// Queue `answer` as the next `prompt_line` response.
    pub fn with_line(mut self, answer: impl Into<String>) -> Self {
        self.lines.push_back(answer.into());
        self
    }

    /// Queue `answer` as the next `prompt_password` response.
    pub fn with_password(mut self, answer: impl Into<String>) -> Self {
        self.passwords.push_back(answer.into());
        self
    }

    /// Queue `answer` as the next `confirm` response.
    pub fn with_confirm(mut self, answer: bool) -> Self {
        self.confirms.push_back(answer);
        self
    }
}

#[cfg(test)]
impl Prompter for FakePrompter {
    fn prompt_line(&mut self, message: &str, default: &str) -> io::Result<String> {
        self.messages.push(message.to_string());
        Ok(self
            .lines
            .pop_front()
            .unwrap_or_else(|| default.to_string()))
    }

    fn prompt_password(&mut self, message: &str) -> io::Result<String> {
        self.messages.push(message.to_string());
        Ok(self
            .passwords
            .pop_front()
            .expect("FakePrompter: no queued password answer"))
    }

    fn confirm(&mut self, message: &str) -> io::Result<bool> {
        self.messages.push(message.to_string());
        Ok(self.confirms.pop_front().unwrap_or(false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_auth_login() {
        let cli = Cli::try_parse_from(["tm", "auth", "login"]).expect("should parse");
        assert!(matches!(
            cli.command,
            Some(Command::Auth {
                cmd: AuthCmd::Login
            })
        ));
    }

    #[test]
    fn parses_auth_status() {
        let cli = Cli::try_parse_from(["tm", "auth", "status"]).expect("should parse");
        assert!(matches!(
            cli.command,
            Some(Command::Auth {
                cmd: AuthCmd::Status
            })
        ));
    }

    #[test]
    fn parses_ticket_with_key() {
        let cli = Cli::try_parse_from(["tm", "ticket", "proj-372"]).expect("should parse");
        match cli.command {
            Some(Command::Ticket { key, cmd }) => {
                assert_eq!(key, Some("proj-372".to_string()));
                assert!(cmd.is_none());
            }
            other => panic!("expected Ticket, got {other:?}"),
        }
    }

    #[test]
    fn parses_ticket_create_with_flags() {
        let cli = Cli::try_parse_from([
            "tm", "ticket", "create", "--title", "Fix it", "--body", "Details",
        ])
        .expect("should parse");
        match cli.command {
            Some(Command::Ticket { key, cmd }) => {
                assert_eq!(key, None);
                match cmd {
                    Some(TicketCmd::Create { title, body }) => {
                        assert_eq!(title, Some("Fix it".to_string()));
                        assert_eq!(body, Some("Details".to_string()));
                    }
                    other => panic!("expected TicketCmd::Create, got {other:?}"),
                }
            }
            other => panic!("expected Ticket, got {other:?}"),
        }
    }

    #[test]
    fn parses_ticket_create_with_no_flags() {
        let cli = Cli::try_parse_from(["tm", "ticket", "create"]).expect("should parse");
        match cli.command {
            Some(Command::Ticket {
                key: None,
                cmd: Some(TicketCmd::Create { title, body }),
            }) => {
                assert_eq!(title, None);
                assert_eq!(body, None);
            }
            other => panic!("expected Ticket Create, got {other:?}"),
        }
    }

    #[test]
    fn bare_ticket_parses_with_no_key_and_no_subcommand() {
        // clap itself doesn't reject this shape (key and cmd are both
        // optional so `args_conflicts_with_subcommands` has nothing to
        // conflict with); the CLI layer is responsible for turning "neither
        // given" into an actionable error. See
        // `cli::ticket::dispatch_missing_key_or_subcommand_errors`.
        let cli = Cli::try_parse_from(["tm", "ticket"]).expect("should parse");
        match cli.command {
            Some(Command::Ticket { key, cmd }) => {
                assert!(key.is_none());
                assert!(cmd.is_none());
            }
            other => panic!("expected Ticket, got {other:?}"),
        }
    }

    #[test]
    fn parses_pr_create_with_flags() {
        let cli = Cli::try_parse_from([
            "tm", "pr", "create", "--title", "Fix it", "--body", "Details", "--base", "main",
        ])
        .expect("should parse");
        match cli.command {
            Some(Command::Pr {
                cmd: PrCmd::Create { title, body, base },
            }) => {
                assert_eq!(title, Some("Fix it".to_string()));
                assert_eq!(body, Some("Details".to_string()));
                assert_eq!(base, Some("main".to_string()));
            }
            other => panic!("expected Pr Create, got {other:?}"),
        }
    }

    #[test]
    fn parses_pr_create_with_no_flags() {
        let cli = Cli::try_parse_from(["tm", "pr", "create"]).expect("should parse");
        match cli.command {
            Some(Command::Pr {
                cmd: PrCmd::Create { title, body, base },
            }) => {
                assert_eq!(title, None);
                assert_eq!(body, None);
                assert_eq!(base, None);
            }
            other => panic!("expected Pr Create, got {other:?}"),
        }
    }

    #[test]
    fn parses_pr_status_with_auto_ticket() {
        let cli =
            Cli::try_parse_from(["tm", "pr", "status", "--auto-ticket"]).expect("should parse");
        match cli.command {
            Some(Command::Pr {
                cmd: PrCmd::Status { auto_ticket },
            }) => assert!(auto_ticket),
            other => panic!("expected Pr Status, got {other:?}"),
        }
    }

    #[test]
    fn parses_pr_status_without_auto_ticket() {
        let cli = Cli::try_parse_from(["tm", "pr", "status"]).expect("should parse");
        match cli.command {
            Some(Command::Pr {
                cmd: PrCmd::Status { auto_ticket },
            }) => assert!(!auto_ticket),
            other => panic!("expected Pr Status, got {other:?}"),
        }
    }

    #[test]
    fn parses_board() {
        let cli = Cli::try_parse_from(["tm", "board"]).expect("should parse");
        assert!(matches!(cli.command, Some(Command::Board)));
    }

    #[test]
    fn no_subcommand_is_none() {
        let cli = Cli::try_parse_from(["tm"]).expect("should parse");
        assert!(cli.command.is_none());
    }
}
