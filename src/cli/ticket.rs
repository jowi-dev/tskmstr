//! `tm ticket <KEY>`, `tm ticket create`, `tm ticket transition`, `tm
//! ticket assign`, `tm ticket rank`, `tm ticket link`, `tm ticket unlink`,
//! `tm ticket update`, `tm ticket audit`, and `tm ticket search`.

use std::io::Write;
use std::path::Path;

use regex::Regex;
use thiserror::Error;

use crate::config::Config;
use crate::jira::adf::{adf_to_text, text_to_adf};
use crate::jira::client::{JiraClient, RankAnchor};
use crate::jira::types::CreateLinkRequest;
use crate::runs::session::{SessionEnv, finish_session, register_session};
use crate::runs::{RunStore, RunStoreError};
use crate::ticketing::{
    AssignOutcome, AssignTarget, CreateTicketContext, TicketingContext, TicketingError,
    TransitionOutcome, assign_ticket, associate_ticket, comment_ticket, create_ticket, link_ticket,
    list_links, list_transitions, rank_ticket, search_tickets, transition_ticket, unlink_ticket,
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

    /// `tm ticket link <KEY> (--blocks|--blocked-by) <OTHER>` was given the
    /// same key (after normalization) for both `KEY` and `OTHER`. Rejected
    /// here, before any Jira call, mirroring [`TicketCliError::RankRelativeToSelf`]:
    /// Jira's issue-link semantics for an issue linked to itself are
    /// undefined/unhelpful.
    #[error("cannot link {key} to itself")]
    LinkRelativeToSelf {
        /// The key given for both the ticket to link and its counterpart.
        key: String,
    },

    /// `tm ticket unlink <KEY> <OTHER>` was given the same key (after
    /// normalization) for both `KEY` and `OTHER`. Rejected here, before any
    /// Jira call, mirroring [`TicketCliError::LinkRelativeToSelf`].
    #[error("cannot unlink {key} from itself")]
    UnlinkRelativeToSelf {
        /// The key given for both the ticket to unlink and its counterpart.
        key: String,
    },

    /// Association with the current branch's pull request failed.
    #[error(transparent)]
    Ticketing(#[from] TicketingError),

    /// A [`RunStore`] operation failed (`tm ticket audit --record`, or
    /// reading a previously recorded audit in read mode).
    #[error(transparent)]
    RunStore(#[from] RunStoreError),

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
    /// `--status`: transition target overriding `Config::status_on_create`
    /// for this invocation. Mutually exclusive with `no_transition` at the
    /// clap level.
    pub status: Option<String>,
    /// `--no-transition`: skip any status transition for this invocation,
    /// even if `Config::status_on_create` is set. Mutually exclusive with
    /// `status` at the clap level.
    pub no_transition: bool,
}

/// `tm ticket create`: create a new ticket in the configured default
/// project, with no pull request involved.
///
/// The transition applied after creation is resolved here, in order:
/// `opts.no_transition` (none), else `opts.status` (the `--status`
/// override), else `Config::status_on_create`, else none. This resolved
/// target is passed straight through to [`create_ticket`], which no longer
/// looks at config itself.
///
/// Session registration (see `docs/plans/session-usage.md`): when
/// `session_store` is `Some`, registers a `create`-kind session run for the
/// new ticket's key immediately after [`create_ticket`] succeeds — the
/// first point the new key exists. Registration failures are swallowed
/// (`register_session`'s error contract requires callers to do this): a
/// broken runs DB must never block ticket creation. `session_store` is
/// `None` when the caller couldn't open the runs DB at all (main.rs opens
/// it leniently for exactly this reason) or on a plain terminal invocation
/// with no session id — either way, `create` still succeeds.
pub fn create(
    ctx: &CreateTicketContext,
    opts: &CreateOptions,
    prompter: &mut dyn super::Prompter,
    session_store: Option<&RunStore>,
    session_env: &SessionEnv,
    sessions_dir: &Path,
    out: &mut dyn Write,
) -> Result<(), TicketCliError> {
    let title = match &opts.title {
        Some(title) => title.clone(),
        None => prompter.prompt_line("Ticket title", "")?,
    };
    if title.trim().is_empty() {
        return Err(TicketCliError::TitleRequired);
    }

    let status_target = if opts.no_transition {
        None
    } else {
        opts.status
            .as_deref()
            .or(ctx.config.status_on_create.as_deref())
    };

    let outcome = create_ticket(ctx, &title, opts.body.as_deref(), status_target)?;

    if let Some(store) = session_store {
        let _ = register_session(
            store,
            sessions_dir,
            session_env,
            "create",
            &outcome.issue_key,
        );
    }

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

/// `tm ticket link <KEY> (--blocks <OTHER> | --blocked-by <OTHER>)`: create a
/// `Blocks`-type Jira link between `key` and `other`, or, with neither flag,
/// list `key`'s existing links of any type.
///
/// At most one of `blocks`/`blocked_by` is expected to be `Some` — clap's
/// `ArgGroup` on [`super::TicketCmd::Link`] enforces this before this
/// function is ever called; neither being set means "list mode". Both keys
/// are normalized via [`normalize_key`]; linking `key` to itself is rejected
/// as [`TicketCliError::LinkRelativeToSelf`] before any Jira call is made.
/// Getting the `--blocks`/`--blocked-by` direction backwards would write
/// inverted dependency data into Jira, so double check against
/// [`CreateLinkRequest`]'s doc comment, not intuition: `--blocks OTHER`
/// means `key` is the blocker (`blocker_key: key, blocked_key: other`);
/// `--blocked-by OTHER` means `key` is the blocked issue (`blocker_key:
/// other, blocked_key: key`).
pub fn link(
    jira: &dyn JiraClient,
    key: &str,
    blocks: Option<&str>,
    blocked_by: Option<&str>,
    out: &mut dyn Write,
) -> Result<(), TicketCliError> {
    let normalized = normalize_key(key)?;
    match (blocks, blocked_by) {
        (Some(other), None) => {
            let other = normalize_key(other)?;
            if normalized == other {
                return Err(TicketCliError::LinkRelativeToSelf { key: normalized });
            }
            let req = CreateLinkRequest {
                blocker_key: normalized.clone(),
                blocked_key: other.clone(),
            };
            link_ticket(jira, &normalized, &req)?;
            writeln!(out, "Linked: {normalized} blocks {other}")?;
            Ok(())
        }
        (None, Some(other)) => {
            let other = normalize_key(other)?;
            if normalized == other {
                return Err(TicketCliError::LinkRelativeToSelf { key: normalized });
            }
            let req = CreateLinkRequest {
                blocker_key: other.clone(),
                blocked_key: normalized.clone(),
            };
            link_ticket(jira, &normalized, &req)?;
            writeln!(out, "Linked: {normalized} is blocked by {other}")?;
            Ok(())
        }
        (None, None) => print_links(jira, &normalized, out),
        (Some(_), Some(_)) => {
            unreachable!("clap's ArgGroup guarantees at most one of blocks/blocked_by is set")
        }
    }
}

/// Print `key`'s existing issue links via [`list_links`], for the `tm
/// ticket link <KEY>` (no flag) discovery view.
fn print_links(
    jira: &dyn JiraClient,
    key: &str,
    out: &mut dyn Write,
) -> Result<(), TicketCliError> {
    let listing = list_links(jira, key)?;
    writeln!(out, "{key} links:")?;
    if listing.links.is_empty() {
        writeln!(out, "No links.")?;
    }
    for link in &listing.links {
        if let Some(other) = &link.inward_issue {
            writeln!(
                out,
                "{} {} ({}): {}",
                link.link_type.inward, other.key, other.fields.status.name, other.fields.summary
            )?;
        } else if let Some(other) = &link.outward_issue {
            writeln!(
                out,
                "{} {} ({}): {}",
                link.link_type.outward, other.key, other.fields.status.name, other.fields.summary
            )?;
        }
        // Neither side present: nothing meaningful to render, so skip it
        // rather than printing a blank/garbled line.
    }
    Ok(())
}

/// `tm ticket unlink <KEY> <OTHER>`: remove the `Blocks`-type link(s) between
/// `key` and `other`, regardless of direction — the inverse of [`link`].
///
/// Both keys are normalized via [`normalize_key`]; unlinking `key` from
/// itself is rejected as [`TicketCliError::UnlinkRelativeToSelf`] before any
/// Jira call is made. Every other failure (no `Blocks` link between the
/// pair, a typo'd key, an API error) is a hard error propagated via
/// [`TicketCliError::Ticketing`]. Prints one `Unlinked: ...` line per
/// removed link, in the order [`unlink_ticket`] reports them.
pub fn unlink(
    jira: &dyn JiraClient,
    key: &str,
    other: &str,
    out: &mut dyn Write,
) -> Result<(), TicketCliError> {
    let normalized = normalize_key(key)?;
    let other = normalize_key(other)?;
    if normalized == other {
        return Err(TicketCliError::UnlinkRelativeToSelf { key: normalized });
    }

    let outcome = unlink_ticket(jira, &normalized, &other)?;
    for phrase in &outcome.removed {
        writeln!(out, "Unlinked: {normalized} {phrase} {other}")?;
    }
    Ok(())
}

/// `tm ticket update <KEY> --body <BODY>`: replace `key`'s description with
/// `body`, converted from GitHub-flavored Markdown to ADF via
/// [`text_to_adf`] (the same conversion `tm ticket create --body` uses).
///
/// This replaces the whole description; there is no partial-update form.
/// Like [`transition`]/[`assign`], this is an explicit request, so any Jira
/// API failure (including a 404 for an unknown `key`) is a hard error
/// propagated via [`TicketCliError::Ticketing`] rather than a warning.
pub fn update(
    jira: &dyn JiraClient,
    key: &str,
    body: &str,
    out: &mut dyn Write,
) -> Result<(), TicketCliError> {
    let normalized = normalize_key(key)?;
    let description = text_to_adf(body);
    jira.update_description(&normalized, &description)
        .map_err(TicketingError::Jira)?;
    writeln!(out, "Description updated for {normalized}")?;
    Ok(())
}

/// `tm ticket comment [<KEY>] [--body <TEXT>] [--pr]`: post a comment to a
/// Jira ticket, optionally also to the current branch's pull request.
///
/// `key` is normalized via [`normalize_key`] when given; `.transpose()` turns
/// the `Option<Result<String, _>>` produced by mapping `normalize_key` over
/// it into the `Result<Option<String>, _>` [`comment_ticket`] expects. Prints
/// `Commented on <KEY>` and, when `also_pr` posted a comment,
/// `Commented on PR #<number>` on its own line.
pub fn comment(
    ctx: &TicketingContext,
    key: Option<&str>,
    body_markdown: &str,
    also_pr: bool,
    out: &mut dyn Write,
) -> Result<(), TicketCliError> {
    let normalized_key = key.map(normalize_key).transpose()?;
    let outcome = comment_ticket(ctx, normalized_key.as_deref(), body_markdown, also_pr)?;

    if let Some(issue_key) = &outcome.issue_key {
        writeln!(out, "Commented on {issue_key}")?;
    }
    if let Some(pr_number) = outcome.pr_number {
        writeln!(out, "Commented on PR #{pr_number}")?;
    }

    Ok(())
}

/// `tm ticket search <TEXT>`: search `config`'s default project for open
/// tickets matching `text`, via [`search_tickets`].
///
/// Prints one line per match, `KEY  STATUS  SUMMARY`, in the search result's
/// order (most recently updated first). Prints a friendly "no matches"
/// message and returns `Ok(())` (exit 0) when nothing matches, rather than
/// treating an empty result as an error — an empty sweep is a normal, useful
/// outcome for this command's "check before creating a duplicate" purpose.
/// An empty/all-whitespace `text` surfaces as
/// [`TicketingError::EmptySearchText`] via [`TicketCliError::Ticketing`];
/// any other Jira/config failure is likewise a hard error.
pub fn search(
    jira: &dyn JiraClient,
    config: &Config,
    text: &str,
    out: &mut dyn Write,
) -> Result<(), TicketCliError> {
    let issues = search_tickets(jira, config, text)?;
    if issues.is_empty() {
        writeln!(out, "No open tickets match \"{text}\".")?;
        return Ok(());
    }
    for issue in &issues {
        writeln!(
            out,
            "{}  {}  {}",
            issue.key, issue.fields.status.name, issue.fields.summary
        )?;
    }
    Ok(())
}

/// Backing store status for `tm ticket audit`'s read mode ([`audit_read`]):
/// either a usable [`RunStore`] handle, or the display text of a failed
/// [`RunStore::open`] attempt.
///
/// Read mode degrades to `Last audit: unavailable (<error>)` on the latter
/// rather than failing the whole command, since the Jira data is the
/// primary payload there. Record mode ([`audit_record`]) has no equivalent:
/// an unopenable runs DB is a hard error there, since persisting the
/// verdict is the entire point.
pub enum AuditStoreStatus<'a> {
    /// The runs DB opened successfully.
    Open(&'a RunStore),
    /// The runs DB could not be opened; carries the error's display text.
    Unavailable(String),
}

/// `tm ticket audit <KEY>` (no `--record`): print `KEY`'s raw Jira data —
/// the material for an interactive audit conversation, which is a Claude
/// skill concern out of scope for `tm` itself — plus its last recorded audit
/// verdict and usage.
///
/// Field order: `KEY  <summary>`, `Status: ...`, `Assignee: ...`, `Links:`
/// (a line per existing issue link, in the same `<verb> <key> (<status>):
/// <summary>` style as `tm ticket link <KEY>`'s bare-list rendering; the
/// whole section is omitted when there are no links), `Last audit: ...`,
/// `Last audit usage: ...` (only when the store is open and a finished
/// `audit`-kind run for this ticket recorded parseable `model_usage`; see
/// `docs/plans/session-usage.md`'s "Surfaces" section), a blank line, then
/// the description rendered via [`adf_to_text`] (or `(no description)`).
///
/// Session registration (see `docs/plans/session-usage.md`): when `store`
/// is [`AuditStoreStatus::Open`], registers an `audit`-kind session run for
/// `key` before reading Jira, so tool/agent-usage events flow for the whole
/// audit conversation that follows this command. Registration failures are
/// swallowed (`register_session`'s error contract requires callers to do
/// this) — a broken runs DB or marker directory never blocks this read.
pub fn audit_read(
    jira: &dyn JiraClient,
    store: &AuditStoreStatus,
    key: &str,
    session_env: &SessionEnv,
    sessions_dir: &Path,
    out: &mut dyn Write,
) -> Result<(), TicketCliError> {
    let normalized = normalize_key(key)?;

    if let AuditStoreStatus::Open(store) = store {
        let _ = register_session(store, sessions_dir, session_env, "audit", &normalized);
    }

    let issue = jira.get_issue(&normalized).map_err(TicketingError::Jira)?;

    writeln!(out, "{}  {}", normalized, issue.fields.summary)?;
    writeln!(out, "Status: {}", issue.fields.status.name)?;
    writeln!(
        out,
        "Assignee: {}",
        issue
            .fields
            .assignee
            .as_ref()
            .map(|a| a.display_name.as_str())
            .unwrap_or("unassigned")
    )?;

    if !issue.fields.issue_links.is_empty() {
        writeln!(out, "Links:")?;
        for link in &issue.fields.issue_links {
            if let Some(other) = &link.inward_issue {
                writeln!(
                    out,
                    "{} {} ({}): {}",
                    link.link_type.inward,
                    other.key,
                    other.fields.status.name,
                    other.fields.summary
                )?;
            } else if let Some(other) = &link.outward_issue {
                writeln!(
                    out,
                    "{} {} ({}): {}",
                    link.link_type.outward,
                    other.key,
                    other.fields.status.name,
                    other.fields.summary
                )?;
            }
            // Neither side present: nothing meaningful to render, matching
            // `print_links`'s handling of the same shape.
        }
    }

    match store {
        AuditStoreStatus::Open(store) => {
            match store.latest_audit_for_ticket(&normalized)? {
                Some(audit) => match &audit.notes {
                    Some(notes) => writeln!(
                        out,
                        "Last audit: {} at {} -- {}",
                        audit.verdict, audit.audited_at, notes
                    )?,
                    None => writeln!(out, "Last audit: {} at {}", audit.verdict, audit.audited_at)?,
                },
                None => writeln!(out, "Last audit: never")?,
            }

            if let Some(line) = store
                .latest_finished_run_for_ticket_kind(&normalized, "audit")
                .ok()
                .flatten()
                .and_then(|run| run.model_usage)
                .and_then(|raw| crate::runs::parse_model_usage(&raw))
                .and_then(|usage| crate::runs::format_model_usage_compact(&usage))
            {
                writeln!(out, "Last audit usage: {line}")?;
            }
        }
        AuditStoreStatus::Unavailable(err) => writeln!(out, "Last audit: unavailable ({err})")?,
    }

    writeln!(out)?;
    let description = issue
        .fields
        .description
        .as_ref()
        .map(adf_to_text)
        .filter(|text| !text.is_empty());
    match description {
        Some(text) => writeln!(out, "{text}")?,
        None => writeln!(out, "(no description)")?,
    }

    Ok(())
}

/// `tm ticket audit <KEY> --record <ready|needs-work> [--notes <TEXT>]`:
/// persist an audit verdict, timestamped by the runs DB itself (see
/// [`RunStore::record_audit`]). Never touches Jira, so this works fully
/// offline.
///
/// Session registration (see `docs/plans/session-usage.md`): after
/// recording succeeds, finishes the session's `audit`-kind run
/// ([`RunStatus::Done`]) — recording a verdict is the natural end of an
/// audit conversation. Swallows any [`crate::runs::session::SessionError`]
/// (same contract as [`audit_read`]/[`create`]): a broken runs DB or an
/// already-finished/absent marker never blocks this command's own output.
pub fn audit_record(
    store: &RunStore,
    key: &str,
    verdict: &str,
    notes: Option<&str>,
    session_env: &SessionEnv,
    sessions_dir: &Path,
    out: &mut dyn Write,
) -> Result<(), TicketCliError> {
    let normalized = normalize_key(key)?;
    store.record_audit(&normalized, verdict, notes)?;
    writeln!(out, "Recorded audit for {normalized}: {verdict}")?;
    let _ = finish_session(
        store,
        sessions_dir,
        session_env,
        "audit",
        &normalized,
        crate::runs::RunStatus::Done,
    );
    Ok(())
}

/// Uppercase `key` and validate it looks like a Jira issue key
/// (`^[A-Z][A-Z0-9]+-\d+$`).
///
/// `pub(crate)` so [`crate::cli::ready`] can reuse the same normalization
/// rule rather than duplicating it.
pub(crate) fn normalize_key(key: &str) -> Result<String, TicketCliError> {
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
    use crate::runs::{FinishRun, RunStatus, StartRun};

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
            run_db_path: None,
            review_bots: vec!["cursor[bot]".to_string()],
            board_column_order: Vec::new(),
            work: crate::config::WorkConfig::default(),
        }
    }

    /// A [`SessionEnv`] with no session id, so [`register_session`]/
    /// [`finish_session`] are guaranteed no-ops — the right default for
    /// every test in this module that isn't specifically exercising session
    /// registration. Paired with [`no_sessions_dir`], which is never
    /// touched by a no-op.
    fn no_session_env() -> SessionEnv {
        SessionEnv {
            session_id: None,
            claude_pid: None,
            lane_run_id: None,
            session_run_id: None,
            cwd: std::path::PathBuf::from("/tmp/wt"),
        }
    }

    /// A sessions directory path that [`no_session_env`] guarantees is
    /// never touched.
    fn no_sessions_dir() -> std::path::PathBuf {
        std::path::PathBuf::from("/tmp/unused-sessions-dir")
    }

    /// A [`SessionEnv`] with a session id, for tests that exercise
    /// [`register_session`]/[`finish_session`]'s active path.
    fn session_env_with_id(session_id: &str) -> SessionEnv {
        SessionEnv {
            session_id: Some(session_id.to_string()),
            claude_pid: Some(4242),
            lane_run_id: None,
            session_run_id: None,
            cwd: std::path::PathBuf::from("/tmp/wt"),
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
            ..Default::default()
        };
        let mut prompter = crate::cli::FakePrompter::new();
        let mut out = Vec::new();

        create(
            &ctx,
            &opts,
            &mut prompter,
            None,
            &no_session_env(),
            &no_sessions_dir(),
            &mut out,
        )
        .expect("should succeed");

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

        create(
            &ctx,
            &opts,
            &mut prompter,
            None,
            &no_session_env(),
            &no_sessions_dir(),
            &mut out,
        )
        .expect("should succeed");

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

        let err = create(
            &ctx,
            &opts,
            &mut prompter,
            None,
            &no_session_env(),
            &no_sessions_dir(),
            &mut out,
        )
        .expect_err("should fail");
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
            ..Default::default()
        };
        let mut prompter = crate::cli::FakePrompter::new();
        let mut out = Vec::new();

        create(
            &ctx,
            &opts,
            &mut prompter,
            None,
            &no_session_env(),
            &no_sessions_dir(),
            &mut out,
        )
        .expect("should succeed");

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
            ..Default::default()
        };
        let mut prompter = crate::cli::FakePrompter::new();
        let mut out = Vec::new();

        create(
            &ctx,
            &opts,
            &mut prompter,
            None,
            &no_session_env(),
            &no_sessions_dir(),
            &mut out,
        )
        .expect("should succeed");

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
            ..Default::default()
        };
        let mut prompter = crate::cli::FakePrompter::new();
        let mut out = Vec::new();

        create(
            &ctx,
            &opts,
            &mut prompter,
            None,
            &no_session_env(),
            &no_sessions_dir(),
            &mut out,
        )
        .expect("should succeed");

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
            ..Default::default()
        };
        let mut prompter = crate::cli::FakePrompter::new();
        let mut out = Vec::new();

        create(
            &ctx,
            &opts,
            &mut prompter,
            None,
            &no_session_env(),
            &no_sessions_dir(),
            &mut out,
        )
        .expect("should succeed");

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
            ..Default::default()
        };
        let mut prompter = crate::cli::FakePrompter::new();
        let mut out = Vec::new();

        create(
            &ctx,
            &opts,
            &mut prompter,
            None,
            &no_session_env(),
            &no_sessions_dir(),
            &mut out,
        )
        .expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(!output.contains("Moved"));
        assert!(!output.contains("warning:"));
    }

    #[test]
    fn create_status_override_transitions_to_override_not_status_on_create() {
        let jira = FakeJiraClient::new()
            .with_create_issue_result(issue("PROJ-9"))
            .with_transitions(
                "PROJ-9",
                vec![transition_fixture("21", "Send to review", "In Review")],
            );
        let cfg = Config {
            status_on_create: Some("In Progress".to_string()),
            ..config()
        };
        let ctx = create_ctx(&jira, &cfg);
        let opts = CreateOptions {
            title: Some("Add the widget".to_string()),
            body: None,
            status: Some("In Review".to_string()),
            no_transition: false,
        };
        let mut prompter = crate::cli::FakePrompter::new();
        let mut out = Vec::new();

        create(
            &ctx,
            &opts,
            &mut prompter,
            None,
            &no_session_env(),
            &no_sessions_dir(),
            &mut out,
        )
        .expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("Moved PROJ-9 to In Review"));
        assert_eq!(
            jira.transition_calls(),
            vec![("PROJ-9".to_string(), "21".to_string())]
        );
    }

    #[test]
    fn create_no_transition_skips_status_on_create() {
        let jira = FakeJiraClient::new()
            .with_create_issue_result(issue("PROJ-9"))
            .with_transitions(
                "PROJ-9",
                vec![transition_fixture("11", "Start Progress", "In Progress")],
            );
        let cfg = Config {
            status_on_create: Some("In Progress".to_string()),
            ..config()
        };
        let ctx = create_ctx(&jira, &cfg);
        let opts = CreateOptions {
            title: Some("Add the widget".to_string()),
            body: None,
            status: None,
            no_transition: true,
        };
        let mut prompter = crate::cli::FakePrompter::new();
        let mut out = Vec::new();

        create(
            &ctx,
            &opts,
            &mut prompter,
            None,
            &no_session_env(),
            &no_sessions_dir(),
            &mut out,
        )
        .expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(!output.contains("Moved"));
        assert!(!output.contains("warning:"));
        assert!(jira.transition_calls().is_empty());
    }

    #[test]
    fn create_status_override_prints_warning_when_no_matching_transition() {
        let jira = FakeJiraClient::new().with_create_issue_result(issue("PROJ-9"));
        let cfg = config();
        let ctx = create_ctx(&jira, &cfg);
        let opts = CreateOptions {
            title: Some("Add the widget".to_string()),
            body: None,
            status: Some("In Review".to_string()),
            no_transition: false,
        };
        let mut prompter = crate::cli::FakePrompter::new();
        let mut out = Vec::new();

        create(
            &ctx,
            &opts,
            &mut prompter,
            None,
            &no_session_env(),
            &no_sessions_dir(),
            &mut out,
        )
        .expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("warning:"));
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
    fn update_prints_confirmation_and_calls_update_description_with_adf_body() {
        let jira = FakeJiraClient::new();
        let mut out = Vec::new();

        update(&jira, "proj-372", "**bold** details", &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(output, "Description updated for PROJ-372\n");

        let calls = jira.update_description_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "PROJ-372");
        let description = calls[0].1.to_string();
        assert!(
            description.contains("\"strong\""),
            "markdown body should be converted to ADF marks: {description}"
        );
    }

    #[test]
    fn update_invalid_key_is_an_actionable_error() {
        let jira = FakeJiraClient::new();
        let mut out = Vec::new();

        let err = update(&jira, "not-a-key!", "body", &mut out).expect_err("should fail");
        match err {
            TicketCliError::InvalidKey { key } => assert_eq!(key, "not-a-key!"),
            other => panic!("expected InvalidKey, got {other:?}"),
        }
        assert!(jira.update_description_calls().is_empty());
    }

    #[test]
    fn update_api_error_is_a_hard_error() {
        let jira = FakeJiraClient::new().with_update_description_error(500, "boom");
        let mut out = Vec::new();

        let err = update(&jira, "PROJ-372", "body", &mut out).expect_err("should fail");
        match err {
            TicketCliError::Ticketing(TicketingError::Jira(JiraError::Api { status, message })) => {
                assert_eq!(status, 500);
                assert_eq!(message, "boom");
            }
            other => panic!("expected Jira Api error, got {other:?}"),
        }
        assert!(out.is_empty(), "nothing should be printed on hard failure");
    }

    #[test]
    fn update_not_found_error_passes_through() {
        let jira = FakeJiraClient::new().with_update_description_error(404, "Issue does not exist");
        let mut out = Vec::new();

        let err = update(&jira, "PROJ-404", "body", &mut out).expect_err("should fail");
        match err {
            TicketCliError::Ticketing(TicketingError::Jira(JiraError::Api { status, .. })) => {
                assert_eq!(status, 404)
            }
            other => panic!("expected Jira Api error, got {other:?}"),
        }
    }

    #[test]
    fn comment_explicit_key_prints_confirmation() {
        let jira = FakeJiraClient::new().with_issue("PROJ-372", issue("PROJ-372"));
        let gh = FakeGhCli::new();
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let mut out = Vec::new();

        comment(&ctx, Some("proj-372"), "note", false, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(output, "Commented on PROJ-372\n");
    }

    #[test]
    fn comment_also_pr_prints_both_confirmation_lines() {
        let jira = FakeJiraClient::new().with_issue("PROJ-372", issue("PROJ-372"));
        let gh = FakeGhCli::new().with_pr_view(Ok(Some(pr())));
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let mut out = Vec::new();

        comment(&ctx, Some("PROJ-372"), "note", true, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(output, "Commented on PROJ-372\nCommented on PR #42\n");
    }

    #[test]
    fn comment_no_key_infers_from_current_branch_pr() {
        let jira = FakeJiraClient::new().with_issue("PROJ-372", issue("PROJ-372"));
        let mut pull_request = pr();
        pull_request.title = "[PROJ-372] Fix the thing".to_string();
        pull_request.head_ref_name = "some-other-branch".to_string();
        let gh = FakeGhCli::new().with_pr_view(Ok(Some(pull_request)));
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let mut out = Vec::new();

        comment(&ctx, None, "note", false, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(output, "Commented on PROJ-372\n");
    }

    #[test]
    fn comment_invalid_key_is_an_actionable_error() {
        let jira = FakeJiraClient::new();
        let gh = FakeGhCli::new();
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let mut out = Vec::new();

        let err =
            comment(&ctx, Some("not-a-key!"), "note", false, &mut out).expect_err("should fail");
        match err {
            TicketCliError::InvalidKey { key } => assert_eq!(key, "not-a-key!"),
            other => panic!("expected InvalidKey, got {other:?}"),
        }
        assert!(jira.add_comment_calls().is_empty());
    }

    #[test]
    fn comment_no_key_no_pr_for_branch_is_a_hard_error() {
        let jira = FakeJiraClient::new();
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

        let err = comment(&ctx, None, "note", false, &mut out).expect_err("should fail");
        match err {
            TicketCliError::Ticketing(TicketingError::NoTicketOrPrForBranch { branch }) => {
                assert_eq!(branch, "proj-372-fix")
            }
            other => panic!("expected NoTicketOrPrForBranch, got {other:?}"),
        }
        assert!(out.is_empty(), "nothing should be printed on hard failure");
    }

    #[test]
    fn search_happy_path_prints_key_status_and_summary_per_line() {
        let jira = FakeJiraClient::new().with_search_result(crate::jira::types::SearchResult {
            issues: vec![issue("PROJ-1"), issue("PROJ-2")],
            next_page_token: None,
        });
        let cfg = config();
        let mut out = Vec::new();

        search(&jira, &cfg, "login bug", &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(
            output,
            "PROJ-1  To Do  Fix the thing\nPROJ-2  To Do  Fix the thing\n"
        );
    }

    #[test]
    fn search_with_no_matches_prints_friendly_message() {
        let jira = FakeJiraClient::new().with_search_result(crate::jira::types::SearchResult {
            issues: vec![],
            next_page_token: None,
        });
        let cfg = config();
        let mut out = Vec::new();

        search(&jira, &cfg, "login bug", &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(output, "No open tickets match \"login bug\".\n");
    }

    #[test]
    fn search_rejects_empty_text() {
        let jira = FakeJiraClient::new();
        let cfg = config();
        let mut out = Vec::new();

        let err = search(&jira, &cfg, "   ", &mut out).expect_err("empty text should fail");

        assert!(matches!(
            err,
            TicketCliError::Ticketing(TicketingError::EmptySearchText)
        ));
    }

    #[test]
    fn search_propagates_jira_search_error() {
        let jira = FakeJiraClient::new().with_search_error(500, "boom");
        let cfg = config();
        let mut out = Vec::new();

        let err = search(&jira, &cfg, "login bug", &mut out).expect_err("should fail");

        match err {
            TicketCliError::Ticketing(TicketingError::Jira(JiraError::Api { status, .. })) => {
                assert_eq!(status, 500)
            }
            other => panic!("expected Jira Api error, got {other:?}"),
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

    #[test]
    fn link_blocks_prints_message_and_calls_create_link_with_key_as_blocker() {
        let jira = FakeJiraClient::new().with_issue("PROJ-372", issue("PROJ-372"));
        let mut out = Vec::new();

        link(&jira, "proj-372", Some("proj-1"), None, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(output, "Linked: PROJ-372 blocks PROJ-1\n");
        assert_eq!(
            jira.create_link_calls(),
            vec![crate::jira::types::CreateLinkRequest {
                blocker_key: "PROJ-372".to_string(),
                blocked_key: "PROJ-1".to_string(),
            }]
        );
    }

    #[test]
    fn link_blocked_by_prints_message_and_calls_create_link_with_key_as_blocked() {
        let jira = FakeJiraClient::new().with_issue("PROJ-372", issue("PROJ-372"));
        let mut out = Vec::new();

        link(&jira, "proj-372", None, Some("proj-1"), &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(output, "Linked: PROJ-372 is blocked by PROJ-1\n");
        assert_eq!(
            jira.create_link_calls(),
            vec![crate::jira::types::CreateLinkRequest {
                blocker_key: "PROJ-1".to_string(),
                blocked_key: "PROJ-372".to_string(),
            }]
        );
    }

    #[test]
    fn link_relative_to_self_is_a_usage_error() {
        let jira = FakeJiraClient::new().with_issue("PROJ-372", issue("PROJ-372"));
        let mut out = Vec::new();

        let err = link(&jira, "proj-372", Some("PROJ-372"), None, &mut out)
            .expect_err("linking a ticket to itself should fail before any Jira call is made");

        match err {
            TicketCliError::LinkRelativeToSelf { key } => assert_eq!(key, "PROJ-372"),
            other => panic!("expected LinkRelativeToSelf, got {other:?}"),
        }
        assert!(jira.create_link_calls().is_empty());
        assert!(out.is_empty());
    }

    #[test]
    fn link_invalid_primary_key_is_an_actionable_error() {
        let jira = FakeJiraClient::new();
        let mut out = Vec::new();

        let err =
            link(&jira, "not-a-key!", Some("PROJ-1"), None, &mut out).expect_err("should fail");
        match err {
            TicketCliError::InvalidKey { key } => assert_eq!(key, "not-a-key!"),
            other => panic!("expected InvalidKey, got {other:?}"),
        }
    }

    #[test]
    fn link_invalid_other_key_is_an_actionable_error() {
        let jira = FakeJiraClient::new().with_issue("PROJ-372", issue("PROJ-372"));
        let mut out = Vec::new();

        let err =
            link(&jira, "proj-372", Some("not-a-key!"), None, &mut out).expect_err("should fail");
        match err {
            TicketCliError::InvalidKey { key } => assert_eq!(key, "not-a-key!"),
            other => panic!("expected InvalidKey, got {other:?}"),
        }
    }

    #[test]
    fn link_missing_primary_key_gives_friendly_not_found_error_and_makes_no_create_link_call() {
        let jira = FakeJiraClient::new().with_issue_not_found("PROJ-404");
        let mut out = Vec::new();

        let err = link(&jira, "proj-404", Some("PROJ-1"), None, &mut out).expect_err("should fail");

        match err {
            TicketCliError::Ticketing(TicketingError::Jira(JiraError::NotFound { key })) => {
                assert_eq!(key, "PROJ-404")
            }
            other => panic!("expected Jira NotFound, got {other:?}"),
        }
        assert!(jira.create_link_calls().is_empty());
    }

    #[test]
    fn link_create_link_api_error_surfaces() {
        let jira = FakeJiraClient::new()
            .with_issue("PROJ-372", issue("PROJ-372"))
            .with_create_link_error(500, "boom");
        let mut out = Vec::new();

        let err = link(&jira, "proj-372", Some("PROJ-1"), None, &mut out).expect_err("should fail");

        match err {
            TicketCliError::Ticketing(TicketingError::Jira(JiraError::Api { status, message })) => {
                assert_eq!(status, 500);
                assert_eq!(message, "boom");
            }
            other => panic!("expected Jira Api error, got {other:?}"),
        }
    }

    #[test]
    fn link_list_mode_renders_inward_and_outward_links() {
        let mut with_links = issue("PROJ-372");
        with_links.fields.issue_links = vec![
            crate::jira::types::IssueLink {
                id: "10001".to_string(),
                link_type: crate::jira::types::IssueLinkType {
                    name: "Blocks".to_string(),
                    inward: "is blocked by".to_string(),
                    outward: "blocks".to_string(),
                },
                inward_issue: Some(crate::jira::types::LinkedIssue {
                    key: "PROJ-2".to_string(),
                    fields: crate::jira::types::LinkedIssueFields {
                        summary: "Fix the thing".to_string(),
                        status: Status {
                            name: "In Progress".to_string(),
                            status_category: StatusCategory {
                                key: "indeterminate".to_string(),
                            },
                        },
                    },
                }),
                outward_issue: None,
            },
            crate::jira::types::IssueLink {
                id: "10001".to_string(),
                link_type: crate::jira::types::IssueLinkType {
                    name: "Blocks".to_string(),
                    inward: "is blocked by".to_string(),
                    outward: "blocks".to_string(),
                },
                inward_issue: None,
                outward_issue: Some(crate::jira::types::LinkedIssue {
                    key: "PROJ-3".to_string(),
                    fields: crate::jira::types::LinkedIssueFields {
                        summary: "Ship the widget".to_string(),
                        status: Status {
                            name: "To Do".to_string(),
                            status_category: StatusCategory {
                                key: "new".to_string(),
                            },
                        },
                    },
                }),
            },
        ];
        let jira = FakeJiraClient::new().with_issue("PROJ-372", with_links);
        let mut out = Vec::new();

        link(&jira, "proj-372", None, None, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(
            output,
            "PROJ-372 links:\nis blocked by PROJ-2 (In Progress): Fix the thing\nblocks PROJ-3 (To Do): Ship the widget\n"
        );
    }

    #[test]
    fn link_list_mode_with_no_links_prints_no_links() {
        let jira = FakeJiraClient::new().with_issue("PROJ-372", issue("PROJ-372"));
        let mut out = Vec::new();

        link(&jira, "proj-372", None, None, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(output, "PROJ-372 links:\nNo links.\n");
    }

    #[test]
    fn link_list_mode_skips_entry_with_neither_side_present() {
        let mut with_links = issue("PROJ-372");
        with_links.fields.issue_links = vec![crate::jira::types::IssueLink {
            id: "10001".to_string(),
            link_type: crate::jira::types::IssueLinkType {
                name: "Blocks".to_string(),
                inward: "is blocked by".to_string(),
                outward: "blocks".to_string(),
            },
            inward_issue: None,
            outward_issue: None,
        }];
        let jira = FakeJiraClient::new().with_issue("PROJ-372", with_links);
        let mut out = Vec::new();

        link(&jira, "proj-372", None, None, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(output, "PROJ-372 links:\n");
    }

    fn blocks_issue_link(
        id: &str,
        inward: Option<&str>,
        outward: Option<&str>,
    ) -> crate::jira::types::IssueLink {
        crate::jira::types::IssueLink {
            id: id.to_string(),
            link_type: crate::jira::types::IssueLinkType {
                name: "Blocks".to_string(),
                inward: "is blocked by".to_string(),
                outward: "blocks".to_string(),
            },
            inward_issue: inward.map(|key| crate::jira::types::LinkedIssue {
                key: key.to_string(),
                fields: crate::jira::types::LinkedIssueFields {
                    summary: "Summary".to_string(),
                    status: Status {
                        name: "To Do".to_string(),
                        status_category: StatusCategory {
                            key: "new".to_string(),
                        },
                    },
                },
            }),
            outward_issue: outward.map(|key| crate::jira::types::LinkedIssue {
                key: key.to_string(),
                fields: crate::jira::types::LinkedIssueFields {
                    summary: "Summary".to_string(),
                    status: Status {
                        name: "To Do".to_string(),
                        status_category: StatusCategory {
                            key: "new".to_string(),
                        },
                    },
                },
            }),
        }
    }

    #[test]
    fn unlink_removes_inward_blocks_link_and_prints_exact_message() {
        let mut with_link = issue("PROJ-372");
        with_link.fields.issue_links = vec![blocks_issue_link("10001", Some("PROJ-1"), None)];
        let jira = FakeJiraClient::new().with_issue("PROJ-372", with_link);
        let mut out = Vec::new();

        unlink(&jira, "proj-372", "proj-1", &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(output, "Unlinked: PROJ-372 is blocked by PROJ-1\n");
        assert_eq!(jira.delete_link_calls(), vec!["10001".to_string()]);
    }

    #[test]
    fn unlink_removes_outward_blocks_link_and_prints_exact_message() {
        let mut with_link = issue("PROJ-372");
        with_link.fields.issue_links = vec![blocks_issue_link("10002", None, Some("PROJ-1"))];
        let jira = FakeJiraClient::new().with_issue("PROJ-372", with_link);
        let mut out = Vec::new();

        unlink(&jira, "proj-372", "proj-1", &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(output, "Unlinked: PROJ-372 blocks PROJ-1\n");
        assert_eq!(jira.delete_link_calls(), vec!["10002".to_string()]);
    }

    #[test]
    fn unlink_both_directions_prints_two_lines() {
        let mut with_links = issue("PROJ-372");
        with_links.fields.issue_links = vec![
            blocks_issue_link("10001", Some("PROJ-1"), None),
            blocks_issue_link("10002", None, Some("PROJ-1")),
        ];
        let jira = FakeJiraClient::new().with_issue("PROJ-372", with_links);
        let mut out = Vec::new();

        unlink(&jira, "proj-372", "proj-1", &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(
            output,
            "Unlinked: PROJ-372 is blocked by PROJ-1\nUnlinked: PROJ-372 blocks PROJ-1\n"
        );
        assert_eq!(
            jira.delete_link_calls(),
            vec!["10001".to_string(), "10002".to_string()]
        );
    }

    #[test]
    fn unlink_relative_to_self_is_rejected_with_no_jira_calls() {
        let jira = FakeJiraClient::new().with_issue("PROJ-372", issue("PROJ-372"));
        let mut out = Vec::new();

        let err = unlink(&jira, "proj-372", "PROJ-372", &mut out)
            .expect_err("unlinking a ticket from itself should fail before any Jira call");

        match err {
            TicketCliError::UnlinkRelativeToSelf { key } => assert_eq!(key, "PROJ-372"),
            other => panic!("expected UnlinkRelativeToSelf, got {other:?}"),
        }
        assert!(jira.delete_link_calls().is_empty());
        assert!(out.is_empty());
    }

    #[test]
    fn unlink_invalid_primary_key_is_an_actionable_error() {
        let jira = FakeJiraClient::new();
        let mut out = Vec::new();

        let err = unlink(&jira, "not-a-key!", "PROJ-1", &mut out).expect_err("should fail");
        match err {
            TicketCliError::InvalidKey { key } => assert_eq!(key, "not-a-key!"),
            other => panic!("expected InvalidKey, got {other:?}"),
        }
    }

    #[test]
    fn unlink_invalid_other_key_is_an_actionable_error() {
        let jira = FakeJiraClient::new().with_issue("PROJ-372", issue("PROJ-372"));
        let mut out = Vec::new();

        let err = unlink(&jira, "proj-372", "not-a-key!", &mut out).expect_err("should fail");
        match err {
            TicketCliError::InvalidKey { key } => assert_eq!(key, "not-a-key!"),
            other => panic!("expected InvalidKey, got {other:?}"),
        }
    }

    #[test]
    fn unlink_no_blocks_link_between_pair_surfaces_message_intact() {
        let jira = FakeJiraClient::new().with_issue("PROJ-372", issue("PROJ-372"));
        let mut out = Vec::new();

        let err = unlink(&jira, "proj-372", "proj-1", &mut out).expect_err("should fail");

        match err {
            TicketCliError::Ticketing(TicketingError::NoBlocksLinkBetween {
                key,
                other,
                others,
            }) => {
                assert_eq!(key, "PROJ-372");
                assert_eq!(other, "PROJ-1");
                assert_eq!(others, "");
            }
            other => panic!("expected NoBlocksLinkBetween, got {other:?}"),
        }
    }

    #[test]
    fn unlink_missing_primary_key_gives_friendly_not_found_error() {
        let jira = FakeJiraClient::new().with_issue_not_found("PROJ-404");
        let mut out = Vec::new();

        let err = unlink(&jira, "proj-404", "proj-1", &mut out).expect_err("should fail");

        match err {
            TicketCliError::Ticketing(TicketingError::Jira(JiraError::NotFound { key })) => {
                assert_eq!(key, "PROJ-404")
            }
            other => panic!("expected Jira NotFound, got {other:?}"),
        }
        assert!(jira.delete_link_calls().is_empty());
    }

    fn open_run_store(dir: &std::path::Path) -> RunStore {
        RunStore::open(&dir.join("runs.db")).expect("open should succeed")
    }

    #[test]
    fn audit_read_prints_all_sections_and_never_audited() {
        let mut with_links = issue("PROJ-372");
        with_links.fields.assignee = Some(crate::jira::types::UserRef {
            account_id: "acct-1".to_string(),
            display_name: "Jane Doe".to_string(),
        });
        with_links.fields.description = Some(serde_json::json!({
            "type": "doc",
            "version": 1,
            "content": [
                { "type": "paragraph", "content": [{ "type": "text", "text": "Some details." }] }
            ]
        }));
        with_links.fields.issue_links = vec![blocks_issue_link("10001", Some("PROJ-1"), None)];
        let jira = FakeJiraClient::new().with_issue("PROJ-372", with_links);
        let dir = tempfile::tempdir().unwrap();
        let store = open_run_store(dir.path());
        let status = AuditStoreStatus::Open(&store);
        let mut out = Vec::new();

        audit_read(
            &jira,
            &status,
            "proj-372",
            &no_session_env(),
            &no_sessions_dir(),
            &mut out,
        )
        .expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(
            output,
            "PROJ-372  Fix the thing\n\
             Status: To Do\n\
             Assignee: Jane Doe\n\
             Links:\n\
             is blocked by PROJ-1 (To Do): Summary\n\
             Last audit: never\n\
             \n\
             Some details.\n"
        );
    }

    #[test]
    fn audit_read_unassigned_and_no_links_and_no_description() {
        let jira = FakeJiraClient::new().with_issue("PROJ-372", issue("PROJ-372"));
        let dir = tempfile::tempdir().unwrap();
        let store = open_run_store(dir.path());
        let status = AuditStoreStatus::Open(&store);
        let mut out = Vec::new();

        audit_read(
            &jira,
            &status,
            "PROJ-372",
            &no_session_env(),
            &no_sessions_dir(),
            &mut out,
        )
        .expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("Assignee: unassigned\n"));
        assert!(!output.contains("Links:"));
        assert!(output.contains("(no description)"));
    }

    #[test]
    fn audit_read_prints_last_recorded_audit_with_notes() {
        let jira = FakeJiraClient::new().with_issue("PROJ-372", issue("PROJ-372"));
        let dir = tempfile::tempdir().unwrap();
        let store = open_run_store(dir.path());
        store
            .record_audit("PROJ-372", "ready", Some("looks good"))
            .unwrap();
        let status = AuditStoreStatus::Open(&store);
        let mut out = Vec::new();

        audit_read(
            &jira,
            &status,
            "proj-372",
            &no_session_env(),
            &no_sessions_dir(),
            &mut out,
        )
        .expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("Last audit: ready at "));
        assert!(output.contains(" -- looks good\n"));
    }

    #[test]
    fn audit_read_store_unavailable_degrades_instead_of_failing() {
        let jira = FakeJiraClient::new().with_issue("PROJ-372", issue("PROJ-372"));
        let status = AuditStoreStatus::Unavailable("disk full".to_string());
        let mut out = Vec::new();

        audit_read(
            &jira,
            &status,
            "proj-372",
            &no_session_env(),
            &no_sessions_dir(),
            &mut out,
        )
        .expect("should still succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("Last audit: unavailable (disk full)\n"));
    }

    #[test]
    fn audit_read_invalid_key_is_an_actionable_error() {
        let jira = FakeJiraClient::new();
        let dir = tempfile::tempdir().unwrap();
        let store = open_run_store(dir.path());
        let status = AuditStoreStatus::Open(&store);
        let mut out = Vec::new();

        let err = audit_read(
            &jira,
            &status,
            "not-a-key!",
            &no_session_env(),
            &no_sessions_dir(),
            &mut out,
        )
        .expect_err("should fail");
        match err {
            TicketCliError::InvalidKey { key } => assert_eq!(key, "not-a-key!"),
            other => panic!("expected InvalidKey, got {other:?}"),
        }
    }

    #[test]
    fn audit_read_missing_ticket_gives_friendly_not_found_error() {
        let jira = FakeJiraClient::new().with_issue_not_found("PROJ-404");
        let dir = tempfile::tempdir().unwrap();
        let store = open_run_store(dir.path());
        let status = AuditStoreStatus::Open(&store);
        let mut out = Vec::new();

        let err = audit_read(
            &jira,
            &status,
            "proj-404",
            &no_session_env(),
            &no_sessions_dir(),
            &mut out,
        )
        .expect_err("should fail");
        match err {
            TicketCliError::Ticketing(TicketingError::Jira(JiraError::NotFound { key })) => {
                assert_eq!(key, "PROJ-404")
            }
            other => panic!("expected Jira NotFound, got {other:?}"),
        }
    }

    #[test]
    fn audit_record_inserts_a_row_readable_via_latest_audit_for_ticket() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_run_store(dir.path());
        let mut out = Vec::new();

        audit_record(
            &store,
            "proj-372",
            "ready",
            Some("looks good"),
            &no_session_env(),
            &no_sessions_dir(),
            &mut out,
        )
        .expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(output, "Recorded audit for PROJ-372: ready\n");

        let audit = store
            .latest_audit_for_ticket("PROJ-372")
            .unwrap()
            .expect("expected an audit");
        assert_eq!(audit.verdict, "ready");
        assert_eq!(audit.notes.as_deref(), Some("looks good"));
    }

    #[test]
    fn audit_record_invalid_key_is_an_actionable_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_run_store(dir.path());
        let mut out = Vec::new();

        let err = audit_record(
            &store,
            "not-a-key!",
            "ready",
            None,
            &no_session_env(),
            &no_sessions_dir(),
            &mut out,
        )
        .expect_err("should fail");
        match err {
            TicketCliError::InvalidKey { key } => assert_eq!(key, "not-a-key!"),
            other => panic!("expected InvalidKey, got {other:?}"),
        }
    }

    #[test]
    fn audit_read_prints_last_audit_usage_when_a_finished_audit_run_has_model_usage() {
        let jira = FakeJiraClient::new().with_issue("PROJ-372", issue("PROJ-372"));
        let dir = tempfile::tempdir().unwrap();
        let store = open_run_store(dir.path());
        let run_id = store
            .start_run(&StartRun {
                ticket: "PROJ-372".to_string(),
                lane: "audit".to_string(),
                worktree: "/tmp/wt".to_string(),
                branch: None,
                pid: None,
                kind: "audit".to_string(),
                log_path: None,
            })
            .unwrap();
        store
            .finish_run(
                run_id,
                &FinishRun {
                    status: RunStatus::Done,
                    model_usage: Some(r#"{"claude-sonnet-5":{"outputTokens":58564}}"#.to_string()),
                    ..FinishRun::default()
                },
            )
            .unwrap();
        let status = AuditStoreStatus::Open(&store);
        let mut out = Vec::new();

        audit_read(
            &jira,
            &status,
            "proj-372",
            &no_session_env(),
            &no_sessions_dir(),
            &mut out,
        )
        .expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("Last audit usage: sonnet-5 58.6k out\n"));
    }

    #[test]
    fn audit_read_omits_last_audit_usage_when_no_finished_audit_run_exists() {
        let jira = FakeJiraClient::new().with_issue("PROJ-372", issue("PROJ-372"));
        let dir = tempfile::tempdir().unwrap();
        let store = open_run_store(dir.path());
        let status = AuditStoreStatus::Open(&store);
        let mut out = Vec::new();

        audit_read(
            &jira,
            &status,
            "proj-372",
            &no_session_env(),
            &no_sessions_dir(),
            &mut out,
        )
        .expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(!output.contains("Last audit usage"));
    }

    #[test]
    fn audit_read_omits_last_audit_usage_when_only_a_running_audit_run_exists() {
        // A still-running audit run (e.g. the very session doing this read)
        // must not be mistaken for a finished one with usable model_usage.
        let jira = FakeJiraClient::new().with_issue("PROJ-372", issue("PROJ-372"));
        let dir = tempfile::tempdir().unwrap();
        let store = open_run_store(dir.path());
        store
            .start_run(&StartRun {
                ticket: "PROJ-372".to_string(),
                lane: "audit".to_string(),
                worktree: "/tmp/wt".to_string(),
                branch: None,
                pid: None,
                kind: "audit".to_string(),
                log_path: None,
            })
            .unwrap();
        let status = AuditStoreStatus::Open(&store);
        let mut out = Vec::new();

        audit_read(
            &jira,
            &status,
            "proj-372",
            &no_session_env(),
            &no_sessions_dir(),
            &mut out,
        )
        .expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(!output.contains("Last audit usage"));
    }

    #[test]
    fn audit_read_registers_a_session_run_when_a_session_id_is_present() {
        let jira = FakeJiraClient::new().with_issue("PROJ-372", issue("PROJ-372"));
        let dir = tempfile::tempdir().unwrap();
        let store = open_run_store(dir.path());
        let status = AuditStoreStatus::Open(&store);
        let markers_dir = tempfile::tempdir().unwrap();
        let env = session_env_with_id("sess-1");
        let mut out = Vec::new();

        audit_read(
            &jira,
            &status,
            "proj-372",
            &env,
            markers_dir.path(),
            &mut out,
        )
        .expect("should succeed");

        let runs = store.list_runs().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].ticket, "PROJ-372");
        assert_eq!(runs[0].kind, "audit");
        assert!(markers_dir.path().join("sess-1").exists());
    }

    #[test]
    fn audit_read_registers_no_session_run_without_a_session_id() {
        let jira = FakeJiraClient::new().with_issue("PROJ-372", issue("PROJ-372"));
        let dir = tempfile::tempdir().unwrap();
        let store = open_run_store(dir.path());
        let status = AuditStoreStatus::Open(&store);
        let mut out = Vec::new();

        audit_read(
            &jira,
            &status,
            "proj-372",
            &no_session_env(),
            &no_sessions_dir(),
            &mut out,
        )
        .expect("should succeed");

        assert!(store.list_runs().unwrap().is_empty());
    }

    #[test]
    fn audit_record_finishes_the_session_run_and_removes_the_marker() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_run_store(dir.path());
        let markers_dir = tempfile::tempdir().unwrap();
        let env = session_env_with_id("sess-1");

        // Simulate the read-mode registration that would have happened
        // earlier in the same Claude Code session.
        register_session(&store, markers_dir.path(), &env, "audit", "PROJ-372").unwrap();
        let marker = markers_dir.path().join("sess-1");
        assert!(marker.exists());

        let mut out = Vec::new();
        audit_record(
            &store,
            "proj-372",
            "ready",
            None,
            &env,
            markers_dir.path(),
            &mut out,
        )
        .expect("should succeed");

        let runs = store.list_runs().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, RunStatus::Done);
        assert!(!marker.exists());
    }

    #[test]
    fn create_registers_a_session_run_with_kind_create_when_store_is_present() {
        let jira = FakeJiraClient::new().with_create_issue_result(issue("PROJ-9"));
        let cfg = config();
        let ctx = create_ctx(&jira, &cfg);
        let opts = CreateOptions {
            title: Some("Add the widget".to_string()),
            body: None,
            ..Default::default()
        };
        let mut prompter = crate::cli::FakePrompter::new();
        let dir = tempfile::tempdir().unwrap();
        let store = open_run_store(dir.path());
        let markers_dir = tempfile::tempdir().unwrap();
        let env = session_env_with_id("sess-1");
        let mut out = Vec::new();

        create(
            &ctx,
            &opts,
            &mut prompter,
            Some(&store),
            &env,
            markers_dir.path(),
            &mut out,
        )
        .expect("should succeed");

        let runs = store.list_runs().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].ticket, "PROJ-9");
        assert_eq!(runs[0].kind, "create");
    }

    #[test]
    fn create_succeeds_when_session_store_is_none() {
        let jira = FakeJiraClient::new().with_create_issue_result(issue("PROJ-9"));
        let cfg = config();
        let ctx = create_ctx(&jira, &cfg);
        let opts = CreateOptions {
            title: Some("Add the widget".to_string()),
            body: None,
            ..Default::default()
        };
        let mut prompter = crate::cli::FakePrompter::new();
        let mut out = Vec::new();

        create(
            &ctx,
            &opts,
            &mut prompter,
            None,
            &session_env_with_id("sess-1"),
            &no_sessions_dir(),
            &mut out,
        )
        .expect("should succeed even with no runs store to register a session in");

        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("Created ticket PROJ-9"));
    }
}
