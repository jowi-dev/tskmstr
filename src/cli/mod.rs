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
pub mod backend;
pub mod init;
pub mod pr;
pub mod ready;
pub mod review;
pub mod runs;
pub mod ticket;
pub mod work;

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
    #[command(args_conflicts_with_subcommands = true)]
    Runs {
        /// Filter the listing to runs whose `kind` column matches (e.g.
        /// `lane`, `audit`, `create`); omit to list every kind. Only
        /// meaningful without a subcommand (plain `tm runs`).
        #[arg(long)]
        kind: Option<String>,
        /// Instead of listing individual runs, print cost totals grouped by
        /// bot-findings outcome (not measured / clean / findings) -- "what
        /// did clean PRs cost vs. ones that came back with findings",
        /// restricted to `--kind` when given. Only meaningful without a
        /// subcommand (plain `tm runs`).
        #[arg(long, conflicts_with = "by_retro")]
        by_outcome: bool,
        /// Instead of listing individual runs, print cost totals grouped by
        /// shipped-ticket retro verdict (clean / defect) -- "what did the
        /// runs that shipped defects cost, and what did clean ones cost",
        /// restricted to `--kind` when given. Only meaningful without a
        /// subcommand (plain `tm runs`). See `tm ticket retro` for recording
        /// verdicts.
        #[arg(long, conflicts_with = "by_outcome")]
        by_retro: bool,
        /// Which runs action to perform. Omit to list current runs.
        #[command(subcommand)]
        cmd: Option<RunsCmd>,
    },
    /// Provision and manage lane worktrees and their tmux sessions (see
    /// `docs/plans/runner-port.md`).
    Work {
        /// Which work action to perform.
        #[command(subcommand)]
        cmd: WorkCmd,
    },
    /// Close the code-review loop over a ticket's `vdiff`-captured review
    /// comments (see `docs/plans/board-vdiff-review-loop.md`).
    Review {
        /// Which review action to perform.
        #[command(subcommand)]
        cmd: ReviewCmd,
    },
    /// Manage the configured ticket backend (see
    /// `docs/plans/github-issues-backend.md`).
    Backend {
        /// Which backend action to perform.
        #[command(subcommand)]
        cmd: BackendCmd,
    },
    /// Interactively onboard the current repo: choose a ticket backend,
    /// write `.tskmstr.toml`, scaffold a work lane, create the GitHub
    /// backend's status labels, and set up session assets so `tm board`
    /// works immediately after.
    Init {
        /// Accept the default answer for every question (scripted setup).
        #[arg(long)]
        yes: bool,
    },
}

/// `tm backend` subcommands.
#[derive(Subcommand, Debug)]
pub enum BackendCmd {
    /// Idempotently create the `tm:status/*` labels this repo's board needs,
    /// in the configured `[backend.github].repo`.
    ///
    /// Only meaningful under the GitHub backend; running it under the Jira
    /// backend (Jira has no label taxonomy to create) is an error naming the
    /// configured provider.
    InitLabels,
}

/// `tm review` subcommands.
#[derive(Subcommand, Debug)]
pub enum ReviewCmd {
    /// Dispatch a Claude fix pass over the review comments `vdiff` captured
    /// for KEY's lane-run worktree.
    ///
    /// Resolves KEY's latest `kind = "lane"` run to find its worktree and
    /// branch, exports the comments `vdiff.nvim` captured there via `vdiff
    /// --export-comments`, and — if any were captured — dispatches a
    /// tracked `review-fix` run in that same worktree, on that same branch.
    /// No new worktree, no new branch. Exits with a distinct code and
    /// creates no run row if no comments were captured.
    ///
    /// Interactive by default: the fix pass runs in a `fix` window of the
    /// ticket's `tm-<scope>-<key>` tmux session (a second pass becomes `fix-2`), so
    /// it can be attached to and steered. `--headless` runs the one-shot
    /// `claude -p` pass under a detached supervisor instead, and `--fg` runs
    /// that pass synchronously.
    Fix {
        /// Jira issue key of the ticket whose lane-run worktree holds the
        /// captured review comments, e.g. `PROJ-372` (case-insensitive).
        key: String,
        /// Run the one-shot `claude -p` fix pass under a detached
        /// supervisor, instead of the interactive tmux-hosted default.
        #[arg(long)]
        headless: bool,
        /// Run synchronously in the foreground, waiting for `claude` to
        /// finish before returning. Implies `--headless`.
        #[arg(long)]
        fg: bool,
    },
}

/// `tm work` subcommands.
#[derive(Subcommand, Debug)]
pub enum WorkCmd {
    /// Provision (if needed) a lane's worktree and start/attach its tmux
    /// session.
    ///
    /// `NAME` resolves to a configured lane's `repo` if one matches;
    /// otherwise the repo is resolved from the current working directory
    /// (see `tskmstr::cli::work` module docs).
    New {
        /// Lane/worktree name.
        name: String,
        /// Branch to create/attach, if different from `NAME`.
        branch: Option<String>,
        /// Base branch to cut a new branch from, if the branch doesn't
        /// already exist locally or on `origin`.
        #[arg(long)]
        from: Option<String>,
    },
    /// Kill the worktree's tmux session (if any) and remove the worktree.
    Remove {
        /// Lane/worktree name.
        name: String,
    },
    /// List every current tmux session with a worktree/session kind
    /// column.
    List,
    /// Recreate tmux sessions for every existing worktree that doesn't
    /// already have one running (e.g. after a reboot).
    Restore,
    /// Rebuild a ticket's `tm-<scope>-<key>` tmux session and its windows from the
    /// ticket's recorded runs — after a reboot, a `tmux kill-server`, or an
    /// accidental `kill-session`.
    ///
    /// Only runs still in flight come back: a headless one reattaches its
    /// log viewer and keeps going, an interactive one comes back as a shell
    /// in its worktree with a printed `claude --resume` line (its `claude`
    /// died with the pane). Finished runs are history — `tm runs show`/`tm
    /// runs logs` are where that lives. Never attaches, and running it
    /// against a healthy session does nothing.
    Session {
        /// Jira ticket key, e.g. `PROJ-123`.
        key: String,
    },
    /// Finish with a ticket: kill its `tm-<scope>-<key>` tmux session and remove its
    /// lane-run worktree.
    ///
    /// One `kill-session` plus one worktree removal, because the ticket's
    /// whole action history lives in that one session. The worktree comes
    /// from the ticket's run rows, and only a path sitting one level below
    /// the configured worktree root is ever removed — an `audit` run's
    /// `[work.audit].dir` is your own checkout and is never touched.
    Clean {
        /// Jira ticket key, e.g. `PROJ-123`.
        key: String,
    },
    /// Attach to (or create) the tmux session for a directory, defaulting
    /// to the current working directory.
    Start {
        /// Directory to start/attach a session for. Defaults to `cwd`.
        dir: Option<String>,
    },
    /// Run one Claude Code session for a configured lane, tracked in the
    /// run-state store.
    ///
    /// Interactive by default: provisioning/preflight run in the foreground
    /// (so errors surface immediately), then `claude` is launched in a
    /// `work` window of the ticket's `tm-<scope>-<key>` tmux session — attach to
    /// steer it mid-run — and this invocation returns the terminal right
    /// away.
    ///
    /// `--headless` instead runs the autonomous one-shot `claude -p` under
    /// a detached supervisor, for CI, cron, and unattended batches. `--fg`
    /// runs that same headless pass synchronously, so this command's exit
    /// code reports the run's outcome.
    Run {
        /// Configured lane name (must match a `[work.lanes.<name>]` entry).
        lane: String,
        /// Ticket key to scope the worktree/branch to and append to the
        /// prompt, if given.
        ticket: Option<String>,
        /// Base branch to cut this run's branch from, overriding the
        /// lane's `base_branch` and the repo's default branch.
        #[arg(long)]
        from: Option<String>,
        /// Driver model override, e.g. `sonnet`.
        #[arg(long)]
        model: Option<String>,
        /// Max-turns budget override for the driver process.
        #[arg(long = "max-turns")]
        max_turns: Option<String>,
        /// Permission mode override for the driver process.
        #[arg(long = "permission-mode")]
        permission_mode: Option<String>,
        /// Prompt file path override, instead of the lane's configured
        /// `prompt_file`/the `~/.claude/prompts/<lane>.md` default.
        #[arg(long)]
        prompt: Option<String>,
        /// Run the autonomous one-shot `claude -p` pass under a detached
        /// supervisor, instead of the interactive tmux-hosted default.
        #[arg(long)]
        headless: bool,
        /// Run synchronously in the foreground, waiting for `claude` to
        /// finish before returning. Implies `--headless`: an interactive
        /// session has no outcome to report back synchronously.
        #[arg(long)]
        fg: bool,
    },
    /// Hidden: the detached run supervisor's own re-exec target. Not a
    /// user-facing command — `tm work run --headless` spawns this
    /// itself (see `src/work/detach.rs`) after provisioning and starting
    /// the tracked run in the foreground; it deserializes `--state-file`'s
    /// `PreparedRun` JSON, spawns `claude`, waits, and finishes the run.
    #[command(name = "__supervise", hide = true)]
    Supervise {
        /// Path to the JSON-serialized
        /// `tskmstr::work::run::PreparedRun` state file written by the
        /// foreground half of `tm work run`.
        #[arg(long = "state-file")]
        state_file: String,
    },
    /// Install/maintain tm's hook scripts outside a lane worktree (see
    /// `tskmstr::work::hooks_install`).
    Hooks {
        /// Which hooks action to perform.
        #[command(subcommand)]
        cmd: HooksCmd,
    },
}

/// `tm work hooks` subcommands.
#[derive(Subcommand, Debug)]
pub enum HooksCmd {
    /// Install tm's telemetry hooks (`Stop`/`SubagentStop` -> `tm-usage.sh`,
    /// `SessionEnd` -> `tm-session-end.sh`) into a user's own Claude Code
    /// settings, so interactive `tm ticket audit`/`tm ticket create`
    /// sessions record usage the same way lane runs already do.
    ///
    /// Deliberately narrow: never installs `guard-delegate.sh` (it would
    /// start denying ordinary edits in every session, not just lane runs)
    /// or the other lane-only scripts. See
    /// `tskmstr::work::hooks_install` module docs for the full rationale.
    Install {
        /// Install at user level (`~/.claude/settings.json`, or
        /// `$CLAUDE_CONFIG_DIR/settings.json`). Currently the only
        /// supported target — required so a future install target can't
        /// silently reuse this flag's default.
        #[arg(long)]
        user: bool,
        /// Print a summary of what would change; touch nothing. Given the
        /// blast radius of editing a settings file shared with unrelated
        /// tools, this is the recommended way to run the command first.
        #[arg(long = "dry-run")]
        dry_run: bool,
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
        /// Transition the new ticket to this workflow status instead of
        /// `status_on_create` from config, matched case-insensitively. Same
        /// advisory warn-and-continue semantics as the config-driven
        /// transition. Conflicts with `--no-transition`.
        #[arg(long, conflicts_with = "no_transition")]
        status: Option<String>,
        /// Create the ticket without applying any status transition, even if
        /// `status_on_create` is configured. Conflicts with `--status`.
        #[arg(long)]
        no_transition: bool,
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
    /// Post a comment to a Jira ticket, optionally also to the current
    /// branch's pull request.
    ///
    /// Body precedence: `--body`, then piped stdin (when stdin isn't a
    /// TTY), then `$EDITOR` as a last resort. `--pr` posts the comment to
    /// the pull request open for the **current branch** — not necessarily
    /// the one associated with `KEY` — as raw Markdown; the Jira comment is
    /// always ADF-converted from the same Markdown body.
    Comment {
        /// Jira issue key, e.g. `PROJ-372` (case-insensitive). Omit to
        /// infer from the current branch's pull request.
        key: Option<String>,
        /// Comment body, as GitHub-flavored Markdown. Falls back to piped
        /// stdin, then `$EDITOR`, if omitted.
        #[arg(long)]
        body: Option<String>,
        /// Also post the comment to the pull request open for the current
        /// branch.
        #[arg(long)]
        pr: bool,
    },
    /// Record a shipped-ticket retro verdict: whether it turned out to be
    /// defective in production. Local SQLite only; never touches Jira.
    ///
    /// Exactly one of `--clean`/`--defect` is required; clap rejects giving
    /// both or neither. `--note` is optional but, if given, must not be
    /// empty or all-whitespace.
    // Same `override_usage` fix as `Assign`/`Rank`: clap's default-derived
    // synopsis for a required `ArgGroup` mixed with a plain positional puts
    // the group before `KEY`, which reads as `KEY` coming second -- wrong,
    // since `KEY` is always the first positional argument. Plain comment
    // (not a doc comment) so it doesn't leak into `--help`.
    #[command(
        group(
            ArgGroup::new("retro_verdict")
                .required(true)
                .args(["clean", "defect"])
        ),
        override_usage = "tm ticket retro <KEY> (--clean|--defect --severity <SEVERITY>) [--note <TEXT>]"
    )]
    Retro {
        /// Jira issue key, e.g. `PROJ-372` (case-insensitive).
        key: String,
        /// Record that the ticket shipped clean, with no known production
        /// defect.
        #[arg(long, group = "retro_verdict")]
        clean: bool,
        /// Record that the ticket shipped a production defect. Requires
        /// `--severity`.
        #[arg(long, group = "retro_verdict")]
        defect: bool,
        /// Defect severity. Required with `--defect`; rejected with
        /// `--clean`.
        // Enforced by `RunStore::record_retro`, not by clap: `--clean`/
        // `--defect` are `ArgAction::SetTrue` flags, which always carry a
        // (possibly false) value, so `requires`/`requires_if` can't express
        // "only when --defect was actually passed" for them. Plain comment
        // (not a doc comment) so the rationale doesn't leak into --help.
        #[arg(long, value_enum)]
        severity: Option<RetroSeverityArg>,
        /// Free-text notes to attach to the recorded verdict. Must not be
        /// empty or all-whitespace if given.
        #[arg(long)]
        note: Option<String>,
    },
    /// Search the configured default project for open tickets matching
    /// `TEXT`.
    ///
    /// Intended for a quick sweep of non-completed tickets — spotting
    /// potential blockers or duplicates — before creating a new one. Prints
    /// one line per match (`KEY  STATUS  SUMMARY`), most recently updated
    /// first, or a friendly "no matches" message if none are found. `TEXT`
    /// must not be empty or all-whitespace.
    Search {
        /// Free text to search for, e.g. a ticket title or keyword.
        text: String,
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

/// Severities accepted by `tm ticket retro --defect --severity`.
#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub enum RetroSeverityArg {
    /// Low-impact defect.
    Minor,
    /// Significant defect, but not critical.
    Major,
    /// Severe defect (e.g. an outage or data issue).
    Critical,
}

impl From<RetroSeverityArg> for crate::runs::RetroSeverity {
    fn from(value: RetroSeverityArg) -> Self {
        match value {
            RetroSeverityArg::Minor => crate::runs::RetroSeverity::Minor,
            RetroSeverityArg::Major => crate::runs::RetroSeverity::Major,
            RetroSeverityArg::Critical => crate::runs::RetroSeverity::Critical,
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
    /// Watch a ticket's open pull request for its configured review bots to
    /// finish, notifying (or launching a cleanup session) once they do. See
    /// `docs/plans/bugbot-watch.md`'s "CLI surface".
    Watch {
        /// Jira issue key, e.g. `PROJ-372` (case-insensitive).
        key: String,
        /// Run the poll loop synchronously in this process instead of
        /// re-exec'ing a detached child. Used internally by the detached
        /// path's re-exec'd child, and directly useful for manual
        /// debugging.
        #[arg(long)]
        foreground: bool,
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
        /// Discriminates what kind of run this is, e.g. `lane`, `audit`,
        /// `create` (see `docs/plans/session-usage.md`).
        #[arg(long, default_value = "lane")]
        kind: String,
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
        /// Per-model token/cost usage as a JSON object, verbatim from
        /// `claude -p`'s `modelUsage` map, e.g.
        /// `{"claude-fable-5":{"inputTokens":146,"outputTokens":58564,
        /// "cacheReadInputTokens":6535803,"cacheCreationInputTokens":203983,
        /// "costUSD":12.996}}`. Must parse as a JSON object; an invalid
        /// value is a hard error.
        #[arg(long)]
        model_usage: Option<String>,
        /// Number of unresolved bot review findings tallied for this run
        /// (see `tm pr watch`'s poll loop). Omit to leave the run's
        /// `findings_count` untouched; pass `0` to record "measured,
        /// clean" -- that is distinct from omitting the flag, which leaves
        /// it `NULL` ("not measured").
        #[arg(long)]
        findings_count: Option<i64>,
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
        /// Print a single JSON object (run, checklist, model usage, tool
        /// counts, and the full event timeline oldest-first with raw detail)
        /// instead of the human-oriented rendering.
        #[arg(long)]
        json: bool,
        /// Restrict to the latest run of this kind (e.g. `lane`, `audit`,
        /// `create`) instead of the latest run of any kind, disambiguating
        /// the case where a session run shadows a lane run.
        #[arg(long)]
        kind: Option<String>,
    },
    /// Print the session id of the latest run of a ticket, for `claude --resume`.
    Resume {
        /// Jira ticket key, e.g. `PROJ-123`.
        ticket: String,
    },
    /// Reopens a finished run so it can be worked (or resumed) again.
    ///
    /// Only runs whose status is terminal (`done`, `failed`, or
    /// `interrupted`) can be reopened; anything else is a hard error. Clears
    /// `ended_at`/`pid`/`heartbeat_at` and moves `status` to `--to` (default
    /// `queued`) -- see [`crate::runs::RunStore::reopen_run`]'s doc comment
    /// for why `queued` rather than `running` is the safer default.
    Reopen {
        /// Jira ticket key or run row id, e.g. `PROJ-123` or `42`.
        ticket_or_id: String,
        /// Restrict ticket lookup to the latest run of this kind (e.g.
        /// `lane`, `audit`, `create`), same disambiguation as `tm runs show
        /// --kind`. Ignored when `ticket_or_id` is a numeric run id.
        #[arg(long)]
        kind: Option<String>,
        /// Status to reopen into.
        #[arg(long, value_enum, default_value = "queued")]
        to: ReopenStatusArg,
    },
    /// Adopts (or starts) a session run for `kind`/`KEY`, for skills invoked
    /// directly rather than through `tm ticket audit`/`create` (e.g.
    /// `/bugbot-triage`). See `docs/plans/bugbot-watch.md`'s "Adoption"
    /// section. No-op when `CLAUDE_CODE_SESSION_ID` is unset.
    Register {
        /// Discriminates what kind of run this is, e.g. `bugbot-cleanup`.
        #[arg(long)]
        kind: String,
        /// Jira ticket key, e.g. `PROJ-123`.
        key: String,
    },
    /// Live board of lane runs (polls the local run db).
    Watch,
    /// Print (or follow) a run's detached-process log file.
    ///
    /// Falls back to the by-convention path for `kind = "review-watch"` when
    /// the run has no recorded `log_path` (every run started before that
    /// column existed) -- this is what makes a `tm pr watch` cron's log
    /// viewable even though it predates this feature.
    Logs {
        /// Jira ticket key or run row id, e.g. `PROJ-123` or `42`.
        ticket_or_id: String,
        /// Restrict ticket lookup to the latest run of this kind (e.g.
        /// `lane`, `audit`, `review-watch`), same disambiguation as `tm runs
        /// show --kind`. Ignored when `ticket_or_id` is a numeric run id.
        #[arg(long)]
        kind: Option<String>,
        /// Number of trailing lines to print when not following.
        #[arg(long, default_value_t = crate::cli::runs::DEFAULT_LOG_TAIL_LINES)]
        tail: usize,
        /// Keep printing new lines as they're appended, like `tail -f`.
        #[arg(long)]
        follow: bool,
    },
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
    /// Ended abnormally, or its outcome could not be determined.
    Interrupted,
}

impl From<FinishStatusArg> for crate::runs::RunStatus {
    fn from(value: FinishStatusArg) -> Self {
        match value {
            FinishStatusArg::Done => crate::runs::RunStatus::Done,
            FinishStatusArg::Failed => crate::runs::RunStatus::Failed,
            FinishStatusArg::Blocked => crate::runs::RunStatus::Blocked,
            FinishStatusArg::Review => crate::runs::RunStatus::Review,
            FinishStatusArg::Interrupted => crate::runs::RunStatus::Interrupted,
        }
    }
}

/// Statuses `tm runs reopen --to` accepts as a reopen target. Mostly the two
/// non-terminal states a reopened run can usefully land in -- reopening to
/// `review` would just recreate a different terminal-ish state instead of
/// making the run actionable again, so it's deliberately excluded, same as
/// the rest of [`FinishStatusArg`]. `Blocked` is the one exception: it's
/// also included as a *repair* target for rows a bug mislabeled `done` (see
/// the `run_agent_and_finish`/`finish_run_from_supervisor` fix this exists
/// alongside) -- moving such a row to `blocked` is restoring its true state,
/// not recreating a fresh terminal one.
#[derive(clap::ValueEnum, Clone, Copy, Debug, Default)]
pub enum ReopenStatusArg {
    /// Queued to run again but not yet started. The default: avoids
    /// `tm runs reap` immediately re-killing the row (see
    /// [`crate::runs::RunStore::reopen_run`]'s doc comment).
    #[default]
    Queued,
    /// Mark it running immediately.
    Running,
    /// Waiting on an external dependency or escalation -- for repairing a
    /// run a bug mislabeled `done` when it was actually blocked.
    Blocked,
}

impl From<ReopenStatusArg> for crate::runs::RunStatus {
    fn from(value: ReopenStatusArg) -> Self {
        match value {
            ReopenStatusArg::Queued => crate::runs::RunStatus::Queued,
            ReopenStatusArg::Running => crate::runs::RunStatus::Running,
            ReopenStatusArg::Blocked => crate::runs::RunStatus::Blocked,
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

    /// Prompt for a yes/no confirmation with an explicit default for an
    /// empty answer, so questions whose sensible answer is "yes" (e.g. the
    /// `tm init` wizard's scaffolding steps) don't inherit [`Self::confirm`]'s
    /// hardcoded "no".
    fn confirm_with_default(&mut self, message: &str, default: bool) -> io::Result<bool>;
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
        self.confirm_with_default(message, false)
    }

    fn confirm_with_default(&mut self, message: &str, default: bool) -> io::Result<bool> {
        let hint = if default { "[Y/n]" } else { "[y/N]" };
        print!("{message} {hint}: ");
        io::stdout().flush()?;

        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        Ok(match line.trim() {
            "" => default,
            answer => matches!(answer, "y" | "Y" | "yes" | "Yes" | "YES"),
        })
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

    fn confirm_with_default(&mut self, message: &str, default: bool) -> io::Result<bool> {
        self.messages.push(message.to_string());
        Ok(self.confirms.pop_front().unwrap_or(default))
    }
}

/// Spawn an editor for multi-line prose input, for callers where
/// [`Prompter::prompt_line`]'s single-line shape doesn't fit — currently
/// `tm ticket comment`'s body, when neither `--body` nor piped stdin was
/// given.
///
/// A trait (rather than a direct `$EDITOR` shell-out), same rationale as
/// [`Prompter`]: so the precedence logic that decides *whether* to fall back
/// to an editor (see `crate::cli::ticket::resolve_comment_body`) can be
/// tested with a canned [`FakeEditorPrompter`] instead of driving a real
/// editor process.
pub trait EditorPrompter {
    /// Open an editor on a scratch file and return what the user saved.
    fn edit(&mut self) -> io::Result<String>;
}

/// [`EditorPrompter`] backed by a real `$EDITOR` process, given a scratch
/// file to edit.
///
/// Falls back to `vi` if `$EDITOR` isn't set. Like [`crate::github::gh_cli::ShellGhCli`]
/// (see its module doc comment), there is no automated end-to-end test of
/// this real implementation — spawning an interactive editor process isn't
/// meaningfully exercisable in a test — so that coverage is deliberately
/// left to manual verification; [`crate::cli::ticket::resolve_comment_body`]'s
/// precedence logic (the part that decides whether an editor is invoked at
/// all) is what's actually under test, via [`FakeEditorPrompter`].
pub struct RealEditorPrompter;

impl EditorPrompter for RealEditorPrompter {
    fn edit(&mut self) -> io::Result<String> {
        let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
        let file = tempfile::NamedTempFile::new()?;
        let path = file.path().to_path_buf();

        let status = std::process::Command::new(&editor).arg(&path).status()?;
        if !status.success() {
            return Err(io::Error::other(format!("`{editor}` exited with {status}")));
        }

        std::fs::read_to_string(&path)
    }
}

/// [`EditorPrompter`] test double: returns a canned answer and records how
/// many times `edit` was called, so a test can assert an editor was (or
/// wasn't) invoked at all.
#[cfg(test)]
pub struct FakeEditorPrompter {
    result: io::Result<String>,
    /// Number of times `edit` was called.
    pub calls: usize,
}

#[cfg(test)]
impl FakeEditorPrompter {
    /// An editor that returns `content` when invoked.
    pub fn with_content(content: impl Into<String>) -> Self {
        Self {
            result: Ok(content.into()),
            calls: 0,
        }
    }

    /// An editor that fails with `message` when invoked.
    pub fn with_error(message: impl Into<String>) -> Self {
        Self {
            result: Err(io::Error::other(message.into())),
            calls: 0,
        }
    }
}

#[cfg(test)]
impl EditorPrompter for FakeEditorPrompter {
    fn edit(&mut self) -> io::Result<String> {
        self.calls += 1;
        match &self.result {
            Ok(content) => Ok(content.clone()),
            Err(err) => Err(io::Error::new(err.kind(), err.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn fake_prompter_confirm_with_default_falls_back_to_default() {
        let mut prompter = FakePrompter::new();
        assert!(
            prompter
                .confirm_with_default("proceed?", true)
                .expect("should answer")
        );
        assert!(
            !prompter
                .confirm_with_default("proceed?", false)
                .expect("should answer")
        );
    }

    #[test]
    fn fake_prompter_confirm_with_default_prefers_queued_answer() {
        let mut prompter = FakePrompter::new().with_confirm(false);
        assert!(
            !prompter
                .confirm_with_default("proceed?", true)
                .expect("should answer")
        );
        assert_eq!(prompter.messages, vec!["proceed?".to_string()]);
    }

    #[test]
    fn parses_init() {
        let cli = Cli::try_parse_from(["tm", "init"]).expect("should parse");
        assert!(matches!(cli.command, Some(Command::Init { yes: false })));
    }

    #[test]
    fn parses_init_yes() {
        let cli = Cli::try_parse_from(["tm", "init", "--yes"]).expect("should parse");
        assert!(matches!(cli.command, Some(Command::Init { yes: true })));
    }

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
                    Some(TicketCmd::Create {
                        title,
                        body,
                        status,
                        no_transition,
                    }) => {
                        assert_eq!(title, Some("Fix it".to_string()));
                        assert_eq!(body, Some("Details".to_string()));
                        assert_eq!(status, None);
                        assert!(!no_transition);
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
                cmd:
                    Some(TicketCmd::Create {
                        title,
                        body,
                        status,
                        no_transition,
                    }),
            }) => {
                assert_eq!(title, None);
                assert_eq!(body, None);
                assert_eq!(status, None);
                assert!(!no_transition);
            }
            other => panic!("expected Ticket Create, got {other:?}"),
        }
    }

    #[test]
    fn parses_ticket_create_with_status_override() {
        let cli = Cli::try_parse_from([
            "tm",
            "ticket",
            "create",
            "--title",
            "Fix it",
            "--status",
            "Ready For Work",
        ])
        .expect("should parse");
        match cli.command {
            Some(Command::Ticket {
                key: None,
                cmd: Some(TicketCmd::Create { status, .. }),
            }) => {
                assert_eq!(status, Some("Ready For Work".to_string()));
            }
            other => panic!("expected TicketCmd::Create, got {other:?}"),
        }
    }

    #[test]
    fn parses_ticket_create_with_no_transition() {
        let cli = Cli::try_parse_from(["tm", "ticket", "create", "--no-transition"])
            .expect("should parse");
        match cli.command {
            Some(Command::Ticket {
                key: None,
                cmd: Some(TicketCmd::Create { no_transition, .. }),
            }) => {
                assert!(no_transition);
            }
            other => panic!("expected TicketCmd::Create, got {other:?}"),
        }
    }

    #[test]
    fn ticket_create_status_and_no_transition_conflict() {
        let err = Cli::try_parse_from([
            "tm",
            "ticket",
            "create",
            "--status",
            "Ready For Work",
            "--no-transition",
        ])
        .expect_err("should be a clap error");
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::ArgumentConflict,
            "unexpected error kind: {err}"
        );
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
    fn parses_ticket_comment_with_key_and_body() {
        let cli = Cli::try_parse_from([
            "tm",
            "ticket",
            "comment",
            "proj-372",
            "--body",
            "looks good",
        ])
        .expect("should parse");
        match cli.command {
            Some(Command::Ticket {
                key: None,
                cmd: Some(TicketCmd::Comment { key, body, pr }),
            }) => {
                assert_eq!(key, Some("proj-372".to_string()));
                assert_eq!(body, Some("looks good".to_string()));
                assert!(!pr);
            }
            other => panic!("expected TicketCmd::Comment, got {other:?}"),
        }
    }

    #[test]
    fn parses_ticket_comment_with_pr_flag() {
        let cli = Cli::try_parse_from(["tm", "ticket", "comment", "proj-372", "--pr"])
            .expect("should parse");
        match cli.command {
            Some(Command::Ticket {
                key: None,
                cmd: Some(TicketCmd::Comment { key, body, pr }),
            }) => {
                assert_eq!(key, Some("proj-372".to_string()));
                assert_eq!(body, None);
                assert!(pr);
            }
            other => panic!("expected TicketCmd::Comment, got {other:?}"),
        }
    }

    #[test]
    fn parses_ticket_comment_with_no_key() {
        let cli = Cli::try_parse_from(["tm", "ticket", "comment", "--body", "looks good"])
            .expect("should parse");
        match cli.command {
            Some(Command::Ticket {
                key: None,
                cmd: Some(TicketCmd::Comment { key, body, pr }),
            }) => {
                assert_eq!(key, None);
                assert_eq!(body, Some("looks good".to_string()));
                assert!(!pr);
            }
            other => panic!("expected TicketCmd::Comment, got {other:?}"),
        }
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
            Some(Command::Runs {
                kind,
                by_outcome,
                by_retro,
                cmd,
            }) => {
                assert!(kind.is_none());
                assert!(!by_outcome);
                assert!(!by_retro);
                assert!(cmd.is_none());
            }
            other => panic!("expected Runs, got {other:?}"),
        }
    }

    #[test]
    fn parses_runs_with_kind_filter() {
        let cli = Cli::try_parse_from(["tm", "runs", "--kind", "audit"]).expect("should parse");
        match cli.command {
            Some(Command::Runs {
                kind,
                by_outcome,
                by_retro,
                cmd,
            }) => {
                assert_eq!(kind, Some("audit".to_string()));
                assert!(!by_outcome);
                assert!(!by_retro);
                assert!(cmd.is_none());
            }
            other => panic!("expected Runs, got {other:?}"),
        }
    }

    #[test]
    fn parses_runs_with_by_outcome_flag() {
        let cli = Cli::try_parse_from(["tm", "runs", "--by-outcome"]).expect("should parse");
        match cli.command {
            Some(Command::Runs {
                kind,
                by_outcome,
                by_retro,
                cmd,
            }) => {
                assert!(kind.is_none());
                assert!(by_outcome);
                assert!(!by_retro);
                assert!(cmd.is_none());
            }
            other => panic!("expected Runs, got {other:?}"),
        }
    }

    #[test]
    fn parses_runs_with_by_retro_flag() {
        let cli = Cli::try_parse_from(["tm", "runs", "--by-retro"]).expect("should parse");
        match cli.command {
            Some(Command::Runs {
                kind,
                by_outcome,
                by_retro,
                cmd,
            }) => {
                assert!(kind.is_none());
                assert!(!by_outcome);
                assert!(by_retro);
                assert!(cmd.is_none());
            }
            other => panic!("expected Runs, got {other:?}"),
        }
    }

    #[test]
    fn rejects_by_outcome_and_by_retro_together() {
        let err = Cli::try_parse_from(["tm", "runs", "--by-outcome", "--by-retro"])
            .expect_err("should reject conflicting flags");
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::ArgumentConflict,
            "unexpected error kind: {err}"
        );
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
            "--kind",
            "audit",
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
                        kind,
                    }),
                ..
            }) => {
                assert_eq!(ticket, "PROJ-123");
                assert_eq!(lane, "backend");
                assert_eq!(worktree, "/tmp/wt");
                assert_eq!(branch, Some("proj-123".to_string()));
                assert_eq!(pid, Some(4242));
                assert_eq!(kind, "audit");
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
                        kind,
                    }),
                ..
            }) => {
                assert_eq!(ticket, "PROJ-123");
                assert_eq!(lane, "backend");
                assert_eq!(worktree, "/tmp/wt");
                assert_eq!(branch, None);
                assert_eq!(pid, None);
                assert_eq!(kind, "lane");
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
                ..
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
    fn parses_runs_reopen_with_to_blocked() {
        // A repair target: a run mislabeled `done` by the supervisor-clobber
        // bug should be reopenable straight to its true `blocked` status.
        let cli = Cli::try_parse_from(["tm", "runs", "reopen", "18", "--to", "blocked"])
            .expect("should parse");
        match cli.command {
            Some(Command::Runs {
                cmd:
                    Some(RunsCmd::Reopen {
                        ticket_or_id, to, ..
                    }),
                ..
            }) => {
                assert_eq!(ticket_or_id, "18");
                assert!(matches!(to, ReopenStatusArg::Blocked));
                assert_eq!(
                    crate::runs::RunStatus::from(to),
                    crate::runs::RunStatus::Blocked
                );
            }
            other => panic!("expected Runs Reopen, got {other:?}"),
        }
    }

    #[test]
    fn runs_reopen_status_review_is_still_a_clap_error() {
        let result = Cli::try_parse_from(["tm", "runs", "reopen", "18", "--to", "review"]);
        assert!(result.is_err(), "review is not a valid reopen target");
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
                ..
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
                ..
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
                ..
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
                ..
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
                cmd: Some(RunsCmd::Show { ticket, json, kind }),
                ..
            }) => {
                assert_eq!(ticket, "PROJ-1");
                assert!(!json);
                assert!(kind.is_none());
            }
            other => panic!("expected Runs Show, got {other:?}"),
        }
    }

    #[test]
    fn parses_runs_show_json_flag() {
        let cli =
            Cli::try_parse_from(["tm", "runs", "show", "PROJ-1", "--json"]).expect("should parse");
        match cli.command {
            Some(Command::Runs {
                cmd: Some(RunsCmd::Show { ticket, json, .. }),
                ..
            }) => {
                assert_eq!(ticket, "PROJ-1");
                assert!(json);
            }
            other => panic!("expected Runs Show, got {other:?}"),
        }
    }

    #[test]
    fn parses_runs_show_with_kind_filter() {
        let cli = Cli::try_parse_from(["tm", "runs", "show", "PROJ-1", "--kind", "audit"])
            .expect("should parse");
        match cli.command {
            Some(Command::Runs {
                cmd: Some(RunsCmd::Show { ticket, kind, .. }),
                ..
            }) => {
                assert_eq!(ticket, "PROJ-1");
                assert_eq!(kind, Some("audit".to_string()));
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
                ..
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
    fn parses_ticket_retro_clean() {
        let cli = Cli::try_parse_from(["tm", "ticket", "retro", "proj-372", "--clean"]).unwrap();
        match cli.command {
            Some(Command::Ticket {
                key: None,
                cmd:
                    Some(TicketCmd::Retro {
                        key,
                        clean,
                        defect,
                        severity,
                        note,
                    }),
            }) => {
                assert_eq!(key, "proj-372".to_string());
                assert!(clean);
                assert!(!defect);
                assert!(severity.is_none());
                assert_eq!(note, None);
            }
            other => panic!("expected TicketCmd::Retro, got {other:?}"),
        }
    }

    #[test]
    fn parses_ticket_retro_defect_with_severity_and_note() {
        let cli = Cli::try_parse_from([
            "tm",
            "ticket",
            "retro",
            "proj-372",
            "--defect",
            "--severity",
            "critical",
            "--note",
            "took down prod",
        ])
        .unwrap();
        match cli.command {
            Some(Command::Ticket {
                key: None,
                cmd:
                    Some(TicketCmd::Retro {
                        key,
                        clean,
                        defect,
                        severity,
                        note,
                    }),
            }) => {
                assert_eq!(key, "proj-372".to_string());
                assert!(!clean);
                assert!(defect);
                assert!(matches!(severity, Some(RetroSeverityArg::Critical)));
                assert_eq!(note, Some("took down prod".to_string()));
            }
            other => panic!("expected TicketCmd::Retro, got {other:?}"),
        }
    }

    #[test]
    fn ticket_retro_without_clean_or_defect_is_a_clap_error() {
        let result = Cli::try_parse_from(["tm", "ticket", "retro", "proj-372"]);
        assert!(
            result.is_err(),
            "retro requires exactly one of --clean/--defect"
        );
    }

    #[test]
    fn ticket_retro_with_both_clean_and_defect_is_a_clap_error() {
        let result = Cli::try_parse_from([
            "tm",
            "ticket",
            "retro",
            "proj-372",
            "--clean",
            "--defect",
            "--severity",
            "minor",
        ]);
        assert!(
            result.is_err(),
            "--clean and --defect are mutually exclusive"
        );
    }

    #[test]
    fn ticket_retro_clean_with_severity_parses_at_the_clap_layer() {
        // clap can't express "only when --defect was passed" for a
        // `SetTrue` bool flag (see `severity`'s doc comment); this pairing
        // is rejected later by `RunStore::record_retro`, not by clap.
        let cli = Cli::try_parse_from([
            "tm",
            "ticket",
            "retro",
            "proj-372",
            "--clean",
            "--severity",
            "minor",
        ])
        .unwrap();
        match cli.command {
            Some(Command::Ticket {
                cmd:
                    Some(TicketCmd::Retro {
                        clean, severity, ..
                    }),
                ..
            }) => {
                assert!(clean);
                assert!(matches!(severity, Some(RetroSeverityArg::Minor)));
            }
            other => panic!("expected TicketCmd::Retro, got {other:?}"),
        }
    }

    #[test]
    fn ticket_retro_defect_without_severity_still_parses_at_the_clap_layer() {
        // clap only enforces severity -> defect, not the reverse; a
        // defect without severity is rejected later by
        // `RunStore::record_retro`, not by clap.
        let cli = Cli::try_parse_from(["tm", "ticket", "retro", "proj-372", "--defect"]).unwrap();
        match cli.command {
            Some(Command::Ticket {
                cmd: Some(TicketCmd::Retro { severity, .. }),
                ..
            }) => assert!(severity.is_none()),
            other => panic!("expected TicketCmd::Retro, got {other:?}"),
        }
    }

    #[test]
    fn ticket_retro_invalid_severity_is_a_clap_error() {
        let result = Cli::try_parse_from([
            "tm",
            "ticket",
            "retro",
            "proj-372",
            "--defect",
            "--severity",
            "bogus",
        ]);
        assert!(result.is_err(), "unknown severity should be a clap error");
    }

    #[test]
    fn parses_work_new_with_no_optional_args() {
        let cli = Cli::try_parse_from(["tm", "work", "new", "my-lane"]).expect("should parse");
        match cli.command {
            Some(Command::Work {
                cmd: WorkCmd::New { name, branch, from },
            }) => {
                assert_eq!(name, "my-lane");
                assert_eq!(branch, None);
                assert_eq!(from, None);
            }
            other => panic!("expected Work New, got {other:?}"),
        }
    }

    #[test]
    fn parses_work_new_with_branch_and_from() {
        let cli = Cli::try_parse_from([
            "tm",
            "work",
            "new",
            "my-lane",
            "custom-branch",
            "--from",
            "origin/staging",
        ])
        .expect("should parse");
        match cli.command {
            Some(Command::Work {
                cmd: WorkCmd::New { name, branch, from },
            }) => {
                assert_eq!(name, "my-lane");
                assert_eq!(branch, Some("custom-branch".to_string()));
                assert_eq!(from, Some("origin/staging".to_string()));
            }
            other => panic!("expected Work New, got {other:?}"),
        }
    }

    #[test]
    fn parses_work_remove() {
        let cli = Cli::try_parse_from(["tm", "work", "remove", "my-lane"]).expect("should parse");
        match cli.command {
            Some(Command::Work {
                cmd: WorkCmd::Remove { name },
            }) => assert_eq!(name, "my-lane"),
            other => panic!("expected Work Remove, got {other:?}"),
        }
    }

    #[test]
    fn parses_work_list() {
        let cli = Cli::try_parse_from(["tm", "work", "list"]).expect("should parse");
        assert!(matches!(
            cli.command,
            Some(Command::Work { cmd: WorkCmd::List })
        ));
    }

    #[test]
    fn parses_work_restore() {
        let cli = Cli::try_parse_from(["tm", "work", "restore"]).expect("should parse");
        assert!(matches!(
            cli.command,
            Some(Command::Work {
                cmd: WorkCmd::Restore
            })
        ));
    }

    #[test]
    fn parses_work_start_with_no_dir() {
        let cli = Cli::try_parse_from(["tm", "work", "start"]).expect("should parse");
        match cli.command {
            Some(Command::Work {
                cmd: WorkCmd::Start { dir },
            }) => assert_eq!(dir, None),
            other => panic!("expected Work Start, got {other:?}"),
        }
    }

    #[test]
    fn parses_work_start_with_dir() {
        let cli =
            Cli::try_parse_from(["tm", "work", "start", "/tmp/some-dir"]).expect("should parse");
        match cli.command {
            Some(Command::Work {
                cmd: WorkCmd::Start { dir },
            }) => assert_eq!(dir, Some("/tmp/some-dir".to_string())),
            other => panic!("expected Work Start, got {other:?}"),
        }
    }

    #[test]
    fn parses_work_supervise_hidden_subcommand() {
        let cli = Cli::try_parse_from([
            "tm",
            "work",
            "__supervise",
            "--state-file",
            "/tmp/mylane-20260101.supervisor.json",
        ])
        .expect("should parse");
        match cli.command {
            Some(Command::Work {
                cmd: WorkCmd::Supervise { state_file },
            }) => assert_eq!(state_file, "/tmp/mylane-20260101.supervisor.json"),
            other => panic!("expected Work Supervise, got {other:?}"),
        }
    }

    #[test]
    fn parses_work_run_with_no_optional_args() {
        let cli = Cli::try_parse_from(["tm", "work", "run", "my-lane"]).expect("should parse");
        match cli.command {
            Some(Command::Work {
                cmd:
                    WorkCmd::Run {
                        lane,
                        ticket,
                        from,
                        model,
                        max_turns,
                        permission_mode,
                        prompt,
                        headless,
                        fg,
                    },
            }) => {
                assert_eq!(lane, "my-lane");
                assert_eq!(ticket, None);
                assert_eq!(from, None);
                assert_eq!(model, None);
                assert_eq!(max_turns, None);
                assert_eq!(permission_mode, None);
                assert_eq!(prompt, None);
                assert!(!headless);
                assert!(!fg);
            }
            other => panic!("expected Work Run, got {other:?}"),
        }
    }

    #[test]
    fn parses_work_run_with_ticket() {
        let cli = Cli::try_parse_from(["tm", "work", "run", "my-lane", "PROJ-123"])
            .expect("should parse");
        match cli.command {
            Some(Command::Work {
                cmd: WorkCmd::Run { lane, ticket, .. },
            }) => {
                assert_eq!(lane, "my-lane");
                assert_eq!(ticket, Some("PROJ-123".to_string()));
            }
            other => panic!("expected Work Run, got {other:?}"),
        }
    }

    #[test]
    fn parses_work_run_with_from() {
        let cli = Cli::try_parse_from(["tm", "work", "run", "my-lane", "--from", "origin/staging"])
            .expect("should parse");
        match cli.command {
            Some(Command::Work {
                cmd: WorkCmd::Run { from, .. },
            }) => assert_eq!(from, Some("origin/staging".to_string())),
            other => panic!("expected Work Run, got {other:?}"),
        }
    }

    #[test]
    fn parses_work_run_with_model() {
        let cli = Cli::try_parse_from(["tm", "work", "run", "my-lane", "--model", "sonnet"])
            .expect("should parse");
        match cli.command {
            Some(Command::Work {
                cmd: WorkCmd::Run { model, .. },
            }) => assert_eq!(model, Some("sonnet".to_string())),
            other => panic!("expected Work Run, got {other:?}"),
        }
    }

    #[test]
    fn parses_work_run_with_max_turns() {
        let cli = Cli::try_parse_from(["tm", "work", "run", "my-lane", "--max-turns", "300"])
            .expect("should parse");
        match cli.command {
            Some(Command::Work {
                cmd: WorkCmd::Run { max_turns, .. },
            }) => assert_eq!(max_turns, Some("300".to_string())),
            other => panic!("expected Work Run, got {other:?}"),
        }
    }

    #[test]
    fn parses_work_run_with_permission_mode() {
        let cli =
            Cli::try_parse_from(["tm", "work", "run", "my-lane", "--permission-mode", "plan"])
                .expect("should parse");
        match cli.command {
            Some(Command::Work {
                cmd: WorkCmd::Run {
                    permission_mode, ..
                },
            }) => assert_eq!(permission_mode, Some("plan".to_string())),
            other => panic!("expected Work Run, got {other:?}"),
        }
    }

    #[test]
    fn parses_work_run_with_prompt() {
        let cli =
            Cli::try_parse_from(["tm", "work", "run", "my-lane", "--prompt", "/tmp/custom.md"])
                .expect("should parse");
        match cli.command {
            Some(Command::Work {
                cmd: WorkCmd::Run { prompt, .. },
            }) => assert_eq!(prompt, Some("/tmp/custom.md".to_string())),
            other => panic!("expected Work Run, got {other:?}"),
        }
    }

    #[test]
    fn parses_work_run_with_headless() {
        let cli = Cli::try_parse_from(["tm", "work", "run", "my-lane", "--headless"])
            .expect("should parse");
        match cli.command {
            Some(Command::Work {
                cmd: WorkCmd::Run { headless, fg, .. },
            }) => {
                assert!(headless);
                assert!(!fg, "--headless alone does not wait for the run");
            }
            other => panic!("expected Work Run, got {other:?}"),
        }
    }

    #[test]
    fn parses_review_fix_with_headless() {
        let cli = Cli::try_parse_from(["tm", "review", "fix", "PROJ-1", "--headless"])
            .expect("should parse");
        match cli.command {
            Some(Command::Review {
                cmd: ReviewCmd::Fix { key, headless, fg },
            }) => {
                assert_eq!(key, "PROJ-1");
                assert!(headless);
                assert!(!fg);
            }
            other => panic!("expected Review Fix, got {other:?}"),
        }
    }

    #[test]
    fn parses_work_run_with_fg() {
        let cli =
            Cli::try_parse_from(["tm", "work", "run", "my-lane", "--fg"]).expect("should parse");
        match cli.command {
            Some(Command::Work {
                cmd: WorkCmd::Run { fg, .. },
            }) => assert!(fg),
            other => panic!("expected Work Run, got {other:?}"),
        }
    }

    #[test]
    fn parses_work_run_with_all_flags_combined() {
        let cli = Cli::try_parse_from([
            "tm",
            "work",
            "run",
            "my-lane",
            "PROJ-123",
            "--from",
            "origin/staging",
            "--model",
            "sonnet",
            "--max-turns",
            "300",
            "--permission-mode",
            "plan",
            "--prompt",
            "/tmp/custom.md",
            "--fg",
        ])
        .expect("should parse");
        match cli.command {
            Some(Command::Work {
                cmd:
                    WorkCmd::Run {
                        lane,
                        ticket,
                        from,
                        model,
                        max_turns,
                        permission_mode,
                        prompt,
                        headless,
                        fg,
                    },
            }) => {
                assert_eq!(lane, "my-lane");
                assert_eq!(ticket, Some("PROJ-123".to_string()));
                assert_eq!(from, Some("origin/staging".to_string()));
                assert_eq!(model, Some("sonnet".to_string()));
                assert_eq!(max_turns, Some("300".to_string()));
                assert_eq!(permission_mode, Some("plan".to_string()));
                assert_eq!(prompt, Some("/tmp/custom.md".to_string()));
                assert!(fg);
                assert!(
                    !headless,
                    "--fg alone selects the headless path without the flag                      being passed — see Dispatch::from_flags"
                );
            }
            other => panic!("expected Work Run, got {other:?}"),
        }
    }

    #[test]
    fn work_supervise_is_hidden_from_help() {
        let mut cmd = Cli::command();
        let work_cmd = cmd
            .find_subcommand_mut("work")
            .expect("work subcommand should exist");
        let supervise = work_cmd
            .find_subcommand("__supervise")
            .expect("__supervise subcommand should be registered");
        assert!(supervise.is_hide_set());
    }

    #[test]
    fn parses_runs_watch() {
        let cli = Cli::try_parse_from(["tm", "runs", "watch"]).expect("should parse");
        assert!(matches!(
            cli.command,
            Some(Command::Runs {
                cmd: Some(RunsCmd::Watch),
                ..
            })
        ));
    }

    #[test]
    fn parses_work_hooks_install_user() {
        let cli = Cli::try_parse_from(["tm", "work", "hooks", "install", "--user"])
            .expect("should parse");
        match cli.command {
            Some(Command::Work {
                cmd:
                    WorkCmd::Hooks {
                        cmd: HooksCmd::Install { user, dry_run },
                    },
            }) => {
                assert!(user);
                assert!(!dry_run);
            }
            other => panic!("expected Work Hooks Install, got {other:?}"),
        }
    }

    #[test]
    fn parses_work_hooks_install_user_dry_run() {
        let cli = Cli::try_parse_from(["tm", "work", "hooks", "install", "--user", "--dry-run"])
            .expect("should parse");
        match cli.command {
            Some(Command::Work {
                cmd:
                    WorkCmd::Hooks {
                        cmd: HooksCmd::Install { user, dry_run },
                    },
            }) => {
                assert!(user);
                assert!(dry_run);
            }
            other => panic!("expected Work Hooks Install, got {other:?}"),
        }
    }

    #[test]
    fn parses_work_hooks_install_without_user_flag() {
        // `--user` is currently the only supported target, but it's an
        // opt-in flag (not clap-required) so the runtime error message can
        // stay friendly and forward-compatible with a future `--lane`.
        let cli = Cli::try_parse_from(["tm", "work", "hooks", "install"]).expect("should parse");
        match cli.command {
            Some(Command::Work {
                cmd:
                    WorkCmd::Hooks {
                        cmd: HooksCmd::Install { user, .. },
                    },
            }) => assert!(!user),
            other => panic!("expected Work Hooks Install, got {other:?}"),
        }
    }
}
