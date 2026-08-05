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

use clap::{ArgGroup, Parser, Subcommand};

pub mod auth;
pub mod pr;
pub mod ready;
pub mod runs;
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
    /// List tickets assigned to you that are ready to pick up, or check
    /// whether a specific ticket is ready.
    ///
    /// With no `KEY`, lists tickets assigned to the current user that are in
    /// the "To Do" status category and have no open `Blocks`-type blockers,
    /// in Jira's native backlog rank order. With `KEY` (any assignee, any
    /// status), reports whether that one ticket is ready, exiting non-zero
    /// if it's blocked. Both forms also carry a best-effort, advisory
    /// annotation of unresolved GitHub bot review findings on a ready
    /// ticket's associated pull request, if any; this never blocks
    /// claimability or changes the exit code.
    Ready {
        /// Jira issue key to check, e.g. `PROJ-372` (case-insensitive). Omit
        /// to list your ready tickets instead.
        key: Option<String>,
    },
    /// Open the interactive terminal board.
    Board,
    /// Inspect and record autonomous lane runs (local SQLite; see
    /// docs/decisions/0001-run-state.md).
    Runs {
        /// Which runs action to perform. Omit to list current runs.
        #[command(subcommand)]
        cmd: Option<RunsCmd>,
    },
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
    /// Move a ticket to a workflow status, or list its available
    /// transitions.
    ///
    /// With `STATUS`, moves `KEY` there (a hard error, unlike the advisory
    /// `status_on_pr`/`status_on_create` transitions applied by `tm pr
    /// create`/`tm ticket create`) if no matching transition exists or the
    /// Jira API call fails. Without `STATUS`, lists `KEY`'s available
    /// transitions instead.
    Transition {
        /// Jira issue key, e.g. `PROJ-372` (case-insensitive).
        key: String,
        /// Target workflow status name, matched case-insensitively. Omit to
        /// list available transitions instead of applying one.
        status: Option<String>,
    },
    /// Assign a ticket by name, to the current user, or clear its assignee.
    ///
    /// Exactly one of `NAME`, `--me`, or `--unassign` is required; clap
    /// rejects giving more than one or none at all.
    // `override_usage` is needed because clap's default-derived synopsis for
    // a required `ArgGroup` mixed with a plain positional puts the group
    // before `KEY` regardless of field declaration order
    // (`<NAME|--me|--unassign> <KEY>`), which reads as `KEY` coming second —
    // wrong, since `KEY` is always the first positional argument. This is a
    // plain comment, not a doc comment, so it doesn't leak into `--help`.
    #[command(
        group(
            ArgGroup::new("assignee")
                .required(true)
                .args(["name", "me", "unassign"])
        ),
        override_usage = "tm ticket assign <KEY> [NAME|--me|--unassign]"
    )]
    Assign {
        /// Jira issue key, e.g. `PROJ-372` (case-insensitive).
        key: String,
        /// Display name (or an unambiguous substring of one) of the
        /// assignable user to assign the ticket to.
        #[arg(group = "assignee")]
        name: Option<String>,
        /// Assign to the current user.
        #[arg(long, group = "assignee")]
        me: bool,
        /// Clear the ticket's assignee.
        #[arg(long, group = "assignee")]
        unassign: bool,
    },
    /// Move a ticket above or below another in Jira's native backlog rank.
    ///
    /// Exactly one of `--above`/`--below` is required; clap rejects giving
    /// both or neither.
    // Same `override_usage` fix as `Assign`: clap's default-derived synopsis
    // for a required `ArgGroup` mixed with a plain positional puts the group
    // before `KEY`, which reads as `KEY` coming second -- wrong, since `KEY`
    // is always the first positional argument. Plain comment (not a doc
    // comment) so it doesn't leak into `--help`.
    #[command(
        group(
            ArgGroup::new("rank_direction")
                .required(true)
                .args(["above", "below"])
        ),
        override_usage = "tm ticket rank <KEY> (--above <KEY>|--below <KEY>)"
    )]
    Rank {
        /// Jira issue key to rank, e.g. `PROJ-372` (case-insensitive).
        key: String,
        /// Rank `KEY` above (before) this issue key.
        #[arg(long, group = "rank_direction")]
        above: Option<String>,
        /// Rank `KEY` below (after) this issue key.
        #[arg(long, group = "rank_direction")]
        below: Option<String>,
    },
    /// Create a `Blocks`-type Jira link between two tickets, or, with
    /// neither flag, list `KEY`'s existing links of any type.
    ///
    /// Unlike `Assign`/`Rank`, at most one of `--blocks`/`--blocked-by` is
    /// required (`required = false`): giving neither is a valid request,
    /// meaning "list existing links" rather than "create one". Giving both
    /// is still rejected by the `ArgGroup`.
    #[command(
        group(
            ArgGroup::new("link_direction")
                .required(false)
                .args(["blocks", "blocked_by"])
        )
    )]
    Link {
        /// Jira issue key, e.g. `PROJ-372` (case-insensitive).
        key: String,
        /// `KEY` blocks this other issue key.
        #[arg(long, group = "link_direction")]
        blocks: Option<String>,
        /// `KEY` is blocked by this other issue key.
        #[arg(long, group = "link_direction")]
        blocked_by: Option<String>,
    },
    /// Remove the `Blocks`-type link(s) between two tickets, regardless of
    /// direction — the inverse of `Link`.
    Unlink {
        /// Jira issue key, e.g. `PROJ-372` (case-insensitive).
        key: String,
        /// The other issue key to remove the `Blocks` link with.
        other: String,
    },
    /// Replace a ticket's description.
    ///
    /// `--body` REPLACES the whole description; there is no partial-update
    /// form. Supports GitHub-flavored Markdown, converted to Jira's ADF
    /// format the same way `tm ticket create --body` does.
    Update {
        /// Jira issue key, e.g. `PROJ-372` (case-insensitive).
        key: String,
        /// New ticket description, as GitHub-flavored Markdown. Replaces the
        /// existing description entirely.
        #[arg(long)]
        body: String,
    },
    /// Print a ticket's data for a human + Claude audit conversation, or
    /// record that conversation's verdict.
    ///
    /// Without `--record`, prints `KEY`'s summary, status, assignee, links,
    /// last recorded audit, and description — the raw material for an
    /// interactive audit conversation (a Claude skill, out of scope for
    /// `tm` itself). With `--record`, persists a verdict to the same local
    /// SQLite database `tm runs` uses instead, without touching Jira.
    /// `--notes` is only meaningful alongside `--record`, so it requires it.
    Audit {
        /// Jira issue key, e.g. `PROJ-372` (case-insensitive).
        key: String,
        /// Record an audit verdict instead of printing the ticket's data.
        #[arg(long, value_enum)]
        record: Option<AuditVerdict>,
        /// Free-text notes to attach to the recorded verdict.
        #[arg(long, requires = "record")]
        notes: Option<String>,
    },
}

/// Verdicts accepted by `tm ticket audit --record`.
#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub enum AuditVerdict {
    /// Ready to hand off to an autonomous run.
    Ready,
    /// Needs more work before being handed off.
    NeedsWork,
}

impl AuditVerdict {
    /// Returns the lowercase, hyphenated string this verdict is stored and
    /// printed as (`ready` / `needs-work`), matching clap's derived value
    /// names for this enum.
    pub fn as_str(self) -> &'static str {
        match self {
            AuditVerdict::Ready => "ready",
            AuditVerdict::NeedsWork => "needs-work",
        }
    }
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
    /// Report the pull request open for the current branch, its ticket, and
    /// a summary of any unresolved GitHub bot review findings on it.
    Status {
        /// Automatically create a ticket if none is associated yet, without
        /// prompting.
        #[arg(long)]
        auto_ticket: bool,
    },
}

/// `tm runs` subcommands.
#[derive(Subcommand, Debug)]
pub enum RunsCmd {
    /// Record the start of a lane run; prints the new run id.
    Start {
        /// Jira ticket key the run is working, e.g. `PROJ-123`.
        #[arg(long)]
        ticket: String,
        /// Lane name the run executed in.
        #[arg(long)]
        lane: String,
        /// Filesystem path of the git worktree the run used.
        #[arg(long)]
        worktree: String,
        /// Branch checked out in the worktree, if known.
        #[arg(long)]
        branch: Option<String>,
        /// PID of the runner process, if known.
        #[arg(long)]
        pid: Option<u32>,
    },
    /// Record a run's terminal outcome.
    Finish {
        /// Row id returned by `tm runs start`.
        run_id: i64,
        /// Final status of the run.
        #[arg(long, value_enum)]
        status: FinishStatusArg,
        /// Process exit code, if the run exited normally.
        #[arg(long)]
        exit_code: Option<i32>,
        /// `claude -p` session id, enabling `claude --resume`.
        #[arg(long)]
        session_id: Option<String>,
        /// Reported cost of the run in USD.
        #[arg(long)]
        cost_usd: Option<f64>,
        /// Number of turns the run took.
        #[arg(long)]
        num_turns: Option<i64>,
        /// Escalation text, set when `--status blocked`.
        #[arg(long)]
        blocker: Option<String>,
        /// URL of the pull request the run opened, if any.
        #[arg(long)]
        pr_url: Option<String>,
        /// Filesystem path of the full transcript, if one was captured.
        #[arg(long)]
        transcript: Option<String>,
    },
    /// Appends a telemetry event to a run and bumps its heartbeat.
    Event {
        /// Row id returned by `tm runs start`.
        run_id: i64,
        /// Event kind, e.g. `tool_use` or `stop`.
        #[arg(long)]
        kind: String,
        /// Optional JSON detail payload, validated before it is stored.
        #[arg(long)]
        detail: Option<String>,
    },
    /// Marks abandoned runs (stale heartbeat, dead pid) as failed.
    Reap {
        /// Minutes without a heartbeat before a run counts as stale.
        #[arg(long, default_value_t = 10)]
        stale_after: u64,
    },
    /// Event timeline for the latest run of a ticket.
    Show {
        /// Jira ticket key, e.g. `PROJ-123`.
        ticket: String,
    },
    /// Print the session id of the latest run of a ticket, for `claude --resume`.
    Resume {
        /// Jira ticket key, e.g. `PROJ-123`.
        ticket: String,
    },
    /// Live board of lane runs (polls the local run db).
    Watch,
}

/// Terminal statuses accepted by `tm runs finish` (queued/running are not
/// outcomes, so they're deliberately excluded from this enum).
#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub enum FinishStatusArg {
    /// Finished successfully.
    Done,
    /// Finished with an error.
    Failed,
    /// Waiting on an external dependency or escalation.
    Blocked,
    /// Finished and awaiting human review.
    Review,
}

impl From<FinishStatusArg> for crate::runs::RunStatus {
    fn from(value: FinishStatusArg) -> Self {
        match value {
            FinishStatusArg::Done => crate::runs::RunStatus::Done,
            FinishStatusArg::Failed => crate::runs::RunStatus::Failed,
            FinishStatusArg::Blocked => crate::runs::RunStatus::Blocked,
            FinishStatusArg::Review => crate::runs::RunStatus::Review,
        }
    }
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
    fn parses_ticket_transition_with_status() {
        let cli = Cli::try_parse_from(["tm", "ticket", "transition", "proj-372", "Done"])
            .expect("should parse");
        match cli.command {
            Some(Command::Ticket {
                key: None,
                cmd: Some(TicketCmd::Transition { key, status }),
            }) => {
                assert_eq!(key, "proj-372".to_string());
                assert_eq!(status, Some("Done".to_string()));
            }
            other => panic!("expected TicketCmd::Transition, got {other:?}"),
        }
    }

    #[test]
    fn parses_ticket_transition_without_status() {
        let cli =
            Cli::try_parse_from(["tm", "ticket", "transition", "proj-372"]).expect("should parse");
        match cli.command {
            Some(Command::Ticket {
                key: None,
                cmd: Some(TicketCmd::Transition { key, status }),
            }) => {
                assert_eq!(key, "proj-372".to_string());
                assert_eq!(status, None);
            }
            other => panic!("expected TicketCmd::Transition, got {other:?}"),
        }
    }

    #[test]
    fn parses_ticket_assign_with_name() {
        let cli = Cli::try_parse_from(["tm", "ticket", "assign", "proj-372", "Jane"])
            .expect("should parse");
        match cli.command {
            Some(Command::Ticket {
                key: None,
                cmd:
                    Some(TicketCmd::Assign {
                        key,
                        name,
                        me,
                        unassign,
                    }),
            }) => {
                assert_eq!(key, "proj-372".to_string());
                assert_eq!(name, Some("Jane".to_string()));
                assert!(!me);
                assert!(!unassign);
            }
            other => panic!("expected TicketCmd::Assign, got {other:?}"),
        }
    }

    #[test]
    fn parses_ticket_assign_with_me() {
        let cli = Cli::try_parse_from(["tm", "ticket", "assign", "proj-372", "--me"])
            .expect("should parse");
        match cli.command {
            Some(Command::Ticket {
                key: None,
                cmd:
                    Some(TicketCmd::Assign {
                        key,
                        name,
                        me,
                        unassign,
                    }),
            }) => {
                assert_eq!(key, "proj-372".to_string());
                assert_eq!(name, None);
                assert!(me);
                assert!(!unassign);
            }
            other => panic!("expected TicketCmd::Assign, got {other:?}"),
        }
    }

    #[test]
    fn parses_ticket_assign_with_unassign() {
        let cli = Cli::try_parse_from(["tm", "ticket", "assign", "proj-372", "--unassign"])
            .expect("should parse");
        match cli.command {
            Some(Command::Ticket {
                key: None,
                cmd:
                    Some(TicketCmd::Assign {
                        key,
                        name,
                        me,
                        unassign,
                    }),
            }) => {
                assert_eq!(key, "proj-372".to_string());
                assert_eq!(name, None);
                assert!(!me);
                assert!(unassign);
            }
            other => panic!("expected TicketCmd::Assign, got {other:?}"),
        }
    }

    #[test]
    fn ticket_assign_name_and_me_conflict_is_a_clap_error() {
        let result = Cli::try_parse_from(["tm", "ticket", "assign", "proj-372", "Jane", "--me"]);
        assert!(result.is_err(), "NAME and --me should conflict");
    }

    #[test]
    fn ticket_assign_name_and_unassign_conflict_is_a_clap_error() {
        let result =
            Cli::try_parse_from(["tm", "ticket", "assign", "proj-372", "Jane", "--unassign"]);
        assert!(result.is_err(), "NAME and --unassign should conflict");
    }

    #[test]
    fn ticket_assign_me_and_unassign_conflict_is_a_clap_error() {
        let result =
            Cli::try_parse_from(["tm", "ticket", "assign", "proj-372", "--me", "--unassign"]);
        assert!(result.is_err(), "--me and --unassign should conflict");
    }

    #[test]
    fn ticket_assign_with_none_given_is_a_clap_error() {
        let result = Cli::try_parse_from(["tm", "ticket", "assign", "proj-372"]);
        assert!(
            result.is_err(),
            "giving neither NAME, --me, nor --unassign should be a usage error"
        );
    }

    #[test]
    fn ticket_assign_usage_synopsis_lists_key_before_assignee_group() {
        // clap's default-derived usage for a required ArgGroup mixed with a
        // plain positional put the group first regardless of declaration
        // order: "tm ticket assign <NAME|--me|--unassign> <KEY>", which
        // reads as KEY coming second and is wrong -- KEY is always the first
        // positional. Assert the synopsis actually printed to users (in the
        // "missing key" error) reflects reality.
        let err = Cli::try_parse_from(["tm", "ticket", "assign"]).expect_err("missing everything");
        let rendered = err.to_string();
        assert!(
            rendered.contains("tm ticket assign <KEY> [NAME|--me|--unassign]"),
            "usage synopsis should list KEY first: {rendered}"
        );
    }

    #[test]
    fn ticket_assign_without_key_is_a_clap_error() {
        let result = Cli::try_parse_from(["tm", "ticket", "assign"]);
        assert!(result.is_err(), "assign requires a key");
    }

    #[test]
    fn parses_ticket_rank_with_above() {
        let cli = Cli::try_parse_from(["tm", "ticket", "rank", "proj-372", "--above", "proj-1"])
            .expect("should parse");
        match cli.command {
            Some(Command::Ticket {
                key: None,
                cmd: Some(TicketCmd::Rank { key, above, below }),
            }) => {
                assert_eq!(key, "proj-372".to_string());
                assert_eq!(above, Some("proj-1".to_string()));
                assert_eq!(below, None);
            }
            other => panic!("expected TicketCmd::Rank, got {other:?}"),
        }
    }

    #[test]
    fn parses_ticket_rank_with_below() {
        let cli = Cli::try_parse_from(["tm", "ticket", "rank", "proj-372", "--below", "proj-1"])
            .expect("should parse");
        match cli.command {
            Some(Command::Ticket {
                key: None,
                cmd: Some(TicketCmd::Rank { key, above, below }),
            }) => {
                assert_eq!(key, "proj-372".to_string());
                assert_eq!(above, None);
                assert_eq!(below, Some("proj-1".to_string()));
            }
            other => panic!("expected TicketCmd::Rank, got {other:?}"),
        }
    }

    #[test]
    fn ticket_rank_above_and_below_conflict_is_a_clap_error() {
        let result = Cli::try_parse_from([
            "tm", "ticket", "rank", "proj-372", "--above", "proj-1", "--below", "proj-2",
        ]);
        assert!(result.is_err(), "--above and --below should conflict");
    }

    #[test]
    fn ticket_rank_with_neither_above_nor_below_is_a_clap_error() {
        let result = Cli::try_parse_from(["tm", "ticket", "rank", "proj-372"]);
        assert!(
            result.is_err(),
            "giving neither --above nor --below should be a usage error"
        );
    }

    #[test]
    fn ticket_rank_without_key_is_a_clap_error() {
        let result = Cli::try_parse_from(["tm", "ticket", "rank"]);
        assert!(result.is_err(), "rank requires a key");
    }

    #[test]
    fn ticket_rank_usage_synopsis_lists_key_before_direction_group() {
        let err = Cli::try_parse_from(["tm", "ticket", "rank"]).expect_err("missing everything");
        let rendered = err.to_string();
        assert!(
            rendered.contains("tm ticket rank <KEY> (--above <KEY>|--below <KEY>)"),
            "usage synopsis should list KEY first: {rendered}"
        );
    }

    #[test]
    fn parses_ticket_link_with_blocks() {
        let cli = Cli::try_parse_from(["tm", "ticket", "link", "proj-372", "--blocks", "proj-1"])
            .expect("should parse");
        match cli.command {
            Some(Command::Ticket {
                key: None,
                cmd:
                    Some(TicketCmd::Link {
                        key,
                        blocks,
                        blocked_by,
                    }),
            }) => {
                assert_eq!(key, "proj-372".to_string());
                assert_eq!(blocks, Some("proj-1".to_string()));
                assert_eq!(blocked_by, None);
            }
            other => panic!("expected TicketCmd::Link, got {other:?}"),
        }
    }

    #[test]
    fn parses_ticket_link_with_blocked_by() {
        let cli =
            Cli::try_parse_from(["tm", "ticket", "link", "proj-372", "--blocked-by", "proj-1"])
                .expect("should parse");
        match cli.command {
            Some(Command::Ticket {
                key: None,
                cmd:
                    Some(TicketCmd::Link {
                        key,
                        blocks,
                        blocked_by,
                    }),
            }) => {
                assert_eq!(key, "proj-372".to_string());
                assert_eq!(blocks, None);
                assert_eq!(blocked_by, Some("proj-1".to_string()));
            }
            other => panic!("expected TicketCmd::Link, got {other:?}"),
        }
    }

    #[test]
    fn parses_ticket_link_with_neither_flag_as_list_mode() {
        let cli = Cli::try_parse_from(["tm", "ticket", "link", "proj-372"]).expect("should parse");
        match cli.command {
            Some(Command::Ticket {
                key: None,
                cmd:
                    Some(TicketCmd::Link {
                        key,
                        blocks,
                        blocked_by,
                    }),
            }) => {
                assert_eq!(key, "proj-372".to_string());
                assert_eq!(blocks, None);
                assert_eq!(blocked_by, None);
            }
            other => panic!("expected TicketCmd::Link, got {other:?}"),
        }
    }

    #[test]
    fn ticket_link_blocks_and_blocked_by_conflict_is_a_clap_error() {
        let result = Cli::try_parse_from([
            "tm",
            "ticket",
            "link",
            "proj-372",
            "--blocks",
            "proj-1",
            "--blocked-by",
            "proj-2",
        ]);
        assert!(result.is_err(), "--blocks and --blocked-by should conflict");
    }

    #[test]
    fn ticket_link_without_key_is_a_clap_error() {
        let result = Cli::try_parse_from(["tm", "ticket", "link"]);
        assert!(result.is_err(), "link requires a key");
    }

    #[test]
    fn parses_ticket_unlink_with_both_positionals() {
        let cli = Cli::try_parse_from(["tm", "ticket", "unlink", "proj-372", "proj-1"])
            .expect("should parse");
        match cli.command {
            Some(Command::Ticket {
                key: None,
                cmd: Some(TicketCmd::Unlink { key, other }),
            }) => {
                assert_eq!(key, "proj-372".to_string());
                assert_eq!(other, "proj-1".to_string());
            }
            other => panic!("expected TicketCmd::Unlink, got {other:?}"),
        }
    }

    #[test]
    fn ticket_unlink_missing_other_is_a_clap_error() {
        let result = Cli::try_parse_from(["tm", "ticket", "unlink", "proj-372"]);
        assert!(result.is_err(), "unlink requires both KEY and OTHER");
    }

    #[test]
    fn parses_ticket_update_with_body() {
        let cli = Cli::try_parse_from([
            "tm",
            "ticket",
            "update",
            "proj-372",
            "--body",
            "new description",
        ])
        .expect("should parse");
        match cli.command {
            Some(Command::Ticket {
                key: None,
                cmd: Some(TicketCmd::Update { key, body }),
            }) => {
                assert_eq!(key, "proj-372".to_string());
                assert_eq!(body, "new description".to_string());
            }
            other => panic!("expected TicketCmd::Update, got {other:?}"),
        }
    }

    #[test]
    fn ticket_update_without_body_is_a_clap_error() {
        let result = Cli::try_parse_from(["tm", "ticket", "update", "proj-372"]);
        assert!(result.is_err(), "update requires --body");
    }

    #[test]
    fn ticket_update_without_key_is_a_clap_error() {
        let result = Cli::try_parse_from(["tm", "ticket", "update", "--body", "text"]);
        assert!(result.is_err(), "update requires a positional KEY");
    }

    #[test]
    fn ticket_transition_without_key_is_a_clap_error() {
        let result = Cli::try_parse_from(["tm", "ticket", "transition"]);
        assert!(
            result.is_err(),
            "bare `tm ticket transition` with no key should fail to parse"
        );
    }

    #[test]
    fn bare_ticket_parses_with_no_key_and_no_subcommand() {
        // clap itself doesn't reject this shape (key and cmd are both
        // optional so `args_conflicts_with_subcommands` has nothing to
        // conflict with); `main.rs`'s dispatch is responsible for turning
        // "neither given" into `TicketCliError::KeyOrCreateRequired`.
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
    fn parses_ready_with_no_key() {
        let cli = Cli::try_parse_from(["tm", "ready"]).expect("should parse");
        match cli.command {
            Some(Command::Ready { key }) => assert!(key.is_none()),
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn parses_ready_with_key() {
        let cli = Cli::try_parse_from(["tm", "ready", "proj-20"]).expect("should parse");
        match cli.command {
            Some(Command::Ready { key }) => assert_eq!(key, Some("proj-20".to_string())),
            other => panic!("expected Ready, got {other:?}"),
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

    #[test]
    fn parses_bare_runs_as_list() {
        let cli = Cli::try_parse_from(["tm", "runs"]).expect("should parse");
        match cli.command {
            Some(Command::Runs { cmd }) => assert!(cmd.is_none()),
            other => panic!("expected Runs, got {other:?}"),
        }
    }

    #[test]
    fn parses_runs_start_with_all_flags() {
        let cli = Cli::try_parse_from([
            "tm",
            "runs",
            "start",
            "--ticket",
            "PROJ-123",
            "--lane",
            "backend",
            "--worktree",
            "/tmp/wt",
            "--branch",
            "proj-123",
            "--pid",
            "4242",
        ])
        .expect("should parse");
        match cli.command {
            Some(Command::Runs {
                cmd:
                    Some(RunsCmd::Start {
                        ticket,
                        lane,
                        worktree,
                        branch,
                        pid,
                    }),
            }) => {
                assert_eq!(ticket, "PROJ-123");
                assert_eq!(lane, "backend");
                assert_eq!(worktree, "/tmp/wt");
                assert_eq!(branch, Some("proj-123".to_string()));
                assert_eq!(pid, Some(4242));
            }
            other => panic!("expected Runs Start, got {other:?}"),
        }
    }

    #[test]
    fn parses_runs_start_with_minimal_flags() {
        let cli = Cli::try_parse_from([
            "tm",
            "runs",
            "start",
            "--ticket",
            "PROJ-123",
            "--lane",
            "backend",
            "--worktree",
            "/tmp/wt",
        ])
        .expect("should parse");
        match cli.command {
            Some(Command::Runs {
                cmd:
                    Some(RunsCmd::Start {
                        ticket,
                        lane,
                        worktree,
                        branch,
                        pid,
                    }),
            }) => {
                assert_eq!(ticket, "PROJ-123");
                assert_eq!(lane, "backend");
                assert_eq!(worktree, "/tmp/wt");
                assert_eq!(branch, None);
                assert_eq!(pid, None);
            }
            other => panic!("expected Runs Start, got {other:?}"),
        }
    }

    #[test]
    fn parses_runs_finish_with_status_done() {
        let cli = Cli::try_parse_from(["tm", "runs", "finish", "3", "--status", "done"])
            .expect("should parse");
        match cli.command {
            Some(Command::Runs {
                cmd: Some(RunsCmd::Finish { run_id, status, .. }),
            }) => {
                assert_eq!(run_id, 3);
                assert!(matches!(status, FinishStatusArg::Done));
            }
            other => panic!("expected Runs Finish, got {other:?}"),
        }
    }

    #[test]
    fn runs_finish_status_queued_is_a_clap_error() {
        let result = Cli::try_parse_from(["tm", "runs", "finish", "3", "--status", "queued"]);
        assert!(result.is_err(), "queued is not a valid finish status");
    }

    #[test]
    fn runs_finish_status_running_is_a_clap_error() {
        let result = Cli::try_parse_from(["tm", "runs", "finish", "3", "--status", "running"]);
        assert!(result.is_err(), "running is not a valid finish status");
    }

    #[test]
    fn parses_runs_event_with_detail() {
        let cli = Cli::try_parse_from([
            "tm",
            "runs",
            "event",
            "3",
            "--kind",
            "tool_use",
            "--detail",
            r#"{"file":"a.rs"}"#,
        ])
        .expect("should parse");
        match cli.command {
            Some(Command::Runs {
                cmd:
                    Some(RunsCmd::Event {
                        run_id,
                        kind,
                        detail,
                    }),
            }) => {
                assert_eq!(run_id, 3);
                assert_eq!(kind, "tool_use");
                assert_eq!(detail, Some(r#"{"file":"a.rs"}"#.to_string()));
            }
            other => panic!("expected Runs Event, got {other:?}"),
        }
    }

    #[test]
    fn parses_runs_event_without_detail() {
        let cli = Cli::try_parse_from(["tm", "runs", "event", "3", "--kind", "stop"])
            .expect("should parse");
        match cli.command {
            Some(Command::Runs {
                cmd:
                    Some(RunsCmd::Event {
                        run_id,
                        kind,
                        detail,
                    }),
            }) => {
                assert_eq!(run_id, 3);
                assert_eq!(kind, "stop");
                assert_eq!(detail, None);
            }
            other => panic!("expected Runs Event, got {other:?}"),
        }
    }

    #[test]
    fn parses_runs_reap_with_default_stale_after() {
        let cli = Cli::try_parse_from(["tm", "runs", "reap"]).expect("should parse");
        match cli.command {
            Some(Command::Runs {
                cmd: Some(RunsCmd::Reap { stale_after }),
            }) => {
                assert_eq!(stale_after, 10);
            }
            other => panic!("expected Runs Reap, got {other:?}"),
        }
    }

    #[test]
    fn parses_runs_reap_with_explicit_stale_after() {
        let cli = Cli::try_parse_from(["tm", "runs", "reap", "--stale-after", "0"])
            .expect("should parse");
        match cli.command {
            Some(Command::Runs {
                cmd: Some(RunsCmd::Reap { stale_after }),
            }) => {
                assert_eq!(stale_after, 0);
            }
            other => panic!("expected Runs Reap, got {other:?}"),
        }
    }

    #[test]
    fn parses_runs_show() {
        let cli = Cli::try_parse_from(["tm", "runs", "show", "PROJ-1"]).expect("should parse");
        match cli.command {
            Some(Command::Runs {
                cmd: Some(RunsCmd::Show { ticket }),
            }) => {
                assert_eq!(ticket, "PROJ-1");
            }
            other => panic!("expected Runs Show, got {other:?}"),
        }
    }

    #[test]
    fn parses_runs_resume() {
        let cli = Cli::try_parse_from(["tm", "runs", "resume", "PROJ-1"]).expect("should parse");
        match cli.command {
            Some(Command::Runs {
                cmd: Some(RunsCmd::Resume { ticket }),
            }) => {
                assert_eq!(ticket, "PROJ-1");
            }
            other => panic!("expected Runs Resume, got {other:?}"),
        }
    }

    #[test]
    fn parses_ticket_audit_with_no_record() {
        let cli = Cli::try_parse_from(["tm", "ticket", "audit", "proj-372"]).expect("should parse");
        match cli.command {
            Some(Command::Ticket {
                key: None,
                cmd: Some(TicketCmd::Audit { key, record, notes }),
            }) => {
                assert_eq!(key, "proj-372".to_string());
                assert!(record.is_none());
                assert_eq!(notes, None);
            }
            other => panic!("expected TicketCmd::Audit, got {other:?}"),
        }
    }

    #[test]
    fn parses_ticket_audit_with_record_ready() {
        let cli = Cli::try_parse_from(["tm", "ticket", "audit", "proj-372", "--record", "ready"])
            .expect("should parse");
        match cli.command {
            Some(Command::Ticket {
                key: None,
                cmd: Some(TicketCmd::Audit { key, record, notes }),
            }) => {
                assert_eq!(key, "proj-372".to_string());
                assert!(matches!(record, Some(AuditVerdict::Ready)));
                assert_eq!(notes, None);
            }
            other => panic!("expected TicketCmd::Audit, got {other:?}"),
        }
    }

    #[test]
    fn parses_ticket_audit_with_record_needs_work_and_notes() {
        let cli = Cli::try_parse_from([
            "tm",
            "ticket",
            "audit",
            "proj-372",
            "--record",
            "needs-work",
            "--notes",
            "missing acceptance criteria",
        ])
        .expect("should parse");
        match cli.command {
            Some(Command::Ticket {
                key: None,
                cmd: Some(TicketCmd::Audit { key, record, notes }),
            }) => {
                assert_eq!(key, "proj-372".to_string());
                assert!(matches!(record, Some(AuditVerdict::NeedsWork)));
                assert_eq!(notes, Some("missing acceptance criteria".to_string()));
            }
            other => panic!("expected TicketCmd::Audit, got {other:?}"),
        }
    }

    #[test]
    fn ticket_audit_invalid_verdict_is_a_clap_error() {
        let result =
            Cli::try_parse_from(["tm", "ticket", "audit", "proj-372", "--record", "bogus"]);
        assert!(result.is_err(), "unknown verdict should be a clap error");
    }

    #[test]
    fn ticket_audit_notes_without_record_is_a_clap_error() {
        let result =
            Cli::try_parse_from(["tm", "ticket", "audit", "proj-372", "--notes", "some notes"]);
        assert!(
            result.is_err(),
            "--notes without --record should be a clap error"
        );
    }

    #[test]
    fn ticket_audit_without_key_is_a_clap_error() {
        let result = Cli::try_parse_from(["tm", "ticket", "audit"]);
        assert!(result.is_err(), "audit requires a key");
    }

    #[test]
    fn parses_runs_watch() {
        let cli = Cli::try_parse_from(["tm", "runs", "watch"]).expect("should parse");
        assert!(matches!(
            cli.command,
            Some(Command::Runs {
                cmd: Some(RunsCmd::Watch)
            })
        ));
    }
}
