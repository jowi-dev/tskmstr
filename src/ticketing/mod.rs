//! Orchestration of ticket <-> pull request association, and of ticket
//! creation independent of any pull request.
//!
//! This module ties together [`crate::jira`] and [`crate::github`]: given a
//! Jira issue key and the pull request open for the current branch, it makes
//! the PR title carry the key and posts a Jira remote link pointing at the
//! PR. It does not itself talk to the network; all I/O goes through the
//! [`TicketProvider`] and [`GhCli`] trait objects on [`TicketingContext`].
//!
//! Three functions move a ticket to a configured workflow status after
//! creating or linking it, since Jira's create-issue API can't set status
//! directly: [`auto_create_and_associate`] (a fresh ticket auto-created
//! because a PR is already open), [`associate_existing_ticket_for_pr_create`]
//! (a pre-existing ticket that `tm pr create` links to a newly opened PR),
//! and [`create_ticket`] (a fresh ticket made by `tm ticket create`, with no
//! PR involved at all — see [`CreateTicketContext`], which deliberately has
//! no [`GhCli`] dependency). [`create_ticket`] takes its transition target as
//! a plain `Option<&str>` argument rather than reading
//! [`Config::status_on_create`] itself; resolving that argument from `tm
//! ticket create`'s `--status`/`--no-transition` flags and the config
//! fallback is [`crate::cli::ticket::create`]'s job. All three share the same
//! matching logic via
//! [`apply_status_transition`] and the same advisory contract: a transition
//! problem is reported as a [`StatusTransition::Warning`] and never fails the
//! overall operation, since the ticket was already created/linked by the
//! time a transition is attempted. [`associate_ticket`] (`tm ticket <KEY>`)
//! never transitions a ticket's status, nor does `tm pr status`'s read-only
//! report of an already-associated ticket.
//!
//! [`transition_ticket`] (`tm ticket transition <KEY> <STATUS>`) shares the
//! same matching rule (via the private `find_matching_transition` helper)
//! but is the opposite of advisory: since the command is an explicit request
//! to change status, a mismatch or API failure is a hard error
//! ([`TicketingError::NoMatchingTransition`] or the underlying
//! [`JiraError`]), not a warning. [`list_transitions`] (`tm ticket
//! transition <KEY>` with no status) reports a ticket's current status and
//! available transitions without changing anything.
//!
//! [`assign_ticket`] (`tm ticket assign <KEY> ...`) is the same kind of
//! explicit, hard-error command: resolving a name to no assignable user, or
//! to more than one, is [`TicketingError::NoMatchingAssignee`], not a
//! warning. Its project lookup for name resolution comes from `KEY`'s own
//! prefix (via [`project_key_from_issue_key`]), not [`Config::default_project_key`],
//! so assigning a ticket in another project still works.
//!
//! [`rank_ticket`] (`tm ticket rank <KEY> (--above|--below) <OTHER>`) moves a
//! ticket in Jira's native backlog rank (the `Rank` field, via
//! [`TicketProvider::rank`]) relative to another issue. Like transition and
//! assign, it's explicit: any Jira API failure is a hard error. It verifies
//! `KEY` exists first so a typo there gets a friendly [`JiraError::NotFound`];
//! a typo'd anchor key surfaces from the rank call itself as
//! [`JiraError::RankNotFound`].
//!
//! [`link_ticket`] (`tm ticket link <KEY> (--blocks|--blocked-by) <OTHER>`)
//! creates a `Blocks`-type Jira issue link between two tickets, via
//! [`TicketProvider::create_link`]. Like rank, it verifies `KEY` exists first so
//! a typo there gets a friendly [`JiraError::NotFound`]; a typo'd `OTHER`
//! surfaces from the create-link call itself as [`JiraError::LinkNotFound`].
//! [`list_links`] (`tm ticket link <KEY>` with neither flag) is a read-only
//! discovery view: it lists all of `KEY`'s existing issue links, of any link
//! type, not just `Blocks`.
//!
//! [`unlink_ticket`] (`tm ticket unlink <KEY> <OTHER>`) is the inverse of
//! [`link_ticket`]: it removes the `Blocks`-type link(s) between two
//! tickets, regardless of which one is the inward/outward side, via
//! [`TicketProvider::delete_link`]. No matching `Blocks` link between the pair is
//! a hard error ([`TicketingError::NoBlocksLinkBetween`]), naming any
//! non-`Blocks` link found between them instead.
//!
//! [`search_tickets`] (`tm ticket search <TEXT>`) is a read-only discovery
//! query, unrelated to a pull request: it searches
//! [`Config::default_project_key`] for non-`Done` tickets matching `TEXT`
//! (via [`TicketQuery::Search`]), for a caller to sweep for potential
//! blockers/duplicates before creating a new ticket. An empty/all-whitespace
//! `TEXT` is rejected up front as [`TicketingError::EmptySearchText`], the
//! same "don't send a meaningless match-everything query" rationale as
//! [`TicketingError::EmptyAssigneeName`].
//!
//! [`comment_ticket`] (`tm ticket comment [<KEY>] [--body <TEXT>] [--pr]`)
//! posts a comment to a Jira ticket, handing `body_markdown` to
//! [`TicketProvider::add_comment`] as plain Markdown text -- the provider
//! (Jira today) converts it to whatever wire format it needs internally, the
//! same way [`create_ticket`]/`tm ticket update` do. An
//! omitted `KEY` is resolved from the current branch's pull request via
//! [`resolve_existing_key`], the same as `tm pr create`'s key inference;
//! neither an explicit key nor a resolvable one is
//! [`TicketingError::NoTicketOrPrForBranch`]. `--pr` means the pull request
//! open for the *current branch*, not "the pull request associated with the
//! ticket" — there is no reverse issue-to-PR lookup in this codebase, and
//! every other explicit `tm ticket <KEY>` command already only ever touches
//! the current branch's PR; when set, the comment is also posted to that PR
//! as raw Markdown (GitHub comments are Markdown natively, unlike Jira's ADF
//! requirement). Like `transition`/`assign`/`rank`/`link`/`unlink`/`update`,
//! every failure is a hard error — there is no advisory/warning path here,
//! since nothing has already been created or linked by the time a comment
//! attempt fails.

use thiserror::Error;

pub mod provider;

use crate::config::Config;
use crate::github::gh_cli::{GhCli, GhError, PrEditRequest};
use crate::github::pr::{KeySource, PrInfo, find_issue_key_with_source, with_issue_key_prefix};
use crate::jira::client::{JiraError, RankAnchor};
use crate::jira::types::{
    CreateLinkRequest, Issue, IssueLink, JiraUser, LinkedIssue, RemoteLinkRequest,
};
use crate::ticketing::provider::{NewTicket, TicketProvider, TicketQuery};

/// Dependencies shared by the ticketing orchestration functions that deal
/// with a pull request.
pub struct TicketingContext<'a> {
    /// Ticket provider used to verify issues and post remote links.
    pub jira: &'a dyn TicketProvider,
    /// `gh` CLI wrapper used to look up and edit the current branch's PR.
    pub gh: &'a dyn GhCli,
    /// Resolved configuration (Jira base URL, default project, etc).
    pub config: &'a Config,
}

/// Dependencies for [`create_ticket`].
///
/// Deliberately narrower than [`TicketingContext`]: `tm ticket create` has
/// nothing to do with a pull request, so it has no [`GhCli`] dependency and
/// works the same whether or not the current branch has one.
pub struct CreateTicketContext<'a> {
    /// Ticket provider used to create the issue and apply its status transition.
    pub jira: &'a dyn TicketProvider,
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
    /// Outcome of attempting to move the ticket to [`Config::status_on_pr`],
    /// if configured.
    ///
    /// Always `None` on the `tm ticket <KEY>` path ([`associate_ticket`]),
    /// since that path associates an existing ticket outside of `tm pr
    /// create` and must never change its status. Also `None` when
    /// `status_on_pr` isn't configured, or when
    /// [`associate_existing_ticket_for_pr_create`] finds the ticket already
    /// sitting in the target status (nothing to do, so nothing to report).
    pub status_transition: Option<StatusTransition>,
}

/// Outcome of attempting to move a ticket to a configured target status
/// ([`Config::status_on_pr`] or [`Config::status_on_create`]).
///
/// This is always advisory: a transition problem never fails the overall
/// command, since the ticket was already created (and, on the `tm pr
/// create` paths, linked to the PR) by the time a transition is attempted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusTransition {
    /// The ticket was moved to the named status.
    Applied(String),
    /// No matching transition was found, or the transition API call
    /// failed. Carries a human-readable explanation to surface to the
    /// user as a warning.
    Warning(String),
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

    /// `tm ticket transition <KEY> <STATUS>` found no transition leading to
    /// `target`, by either matching rule (see [`find_matching_transition`]).
    /// Unlike [`apply_status_transition`]'s advisory
    /// [`StatusTransition::Warning`], this is a hard error: the command is
    /// explicit, so a caller (often a script) needs a non-zero exit and
    /// enough detail — the available transitions — to retry with a valid
    /// status.
    #[error("no transition to \"{target}\" found for {key}; {available}")]
    NoMatchingTransition {
        /// The issue key that was to be transitioned.
        key: String,
        /// The requested target status that matched no transition.
        target: String,
        /// Describes the issue's available transitions (`"available
        /// transitions: name -> target status, ..."`), or, when there are
        /// none, says so explicitly instead of leaving a dangling empty
        /// list. See [`format_transitions`].
        available: String,
    },

    /// `tm ticket assign <KEY> <NAME>` found either no assignable user
    /// matching `name` in `key`'s project, or more than one — including two
    /// users sharing the exact same `displayName` (real Jira projects can
    /// have this; account IDs are included in `available` specifically to
    /// disambiguate that case). See [`resolve_assignee_by_name`].
    #[error("no unambiguous assignee match for \"{name}\" on {key}; {available}")]
    NoMatchingAssignee {
        /// The issue key that was to be assigned.
        key: String,
        /// The requested name that matched zero or more than one assignable
        /// user.
        name: String,
        /// Describes the candidates found (`"candidates: name (accountId),
        /// ..."`), or, when none matched, the project's full
        /// assignable-user list (`"assignable users: name (accountId),
        /// ..."`), or, when the project has none at all, says so explicitly.
        /// See [`format_assignee_candidates`].
        available: String,
    },

    /// `tm ticket assign <KEY> <NAME>` was given an empty (or all-whitespace)
    /// `NAME`.
    ///
    /// Rejected explicitly rather than left to the substring-match rule in
    /// [`resolve_assignee_by_name`], which would otherwise treat `""` as
    /// matching every assignable user's `displayName` (`str::contains("")`
    /// is always true) — silently succeeding in any project with exactly one
    /// assignable user instead of failing loudly.
    #[error("assignee name for {key} must not be empty")]
    EmptyAssigneeName {
        /// The issue key that was to be assigned.
        key: String,
    },

    /// `tm ticket search <TEXT>` was given an empty (or all-whitespace)
    /// `TEXT`.
    ///
    /// Rejected explicitly rather than left to Jira's `text ~ ""` clause,
    /// whose behavior is unhelpful/undefined for tskmstr's purposes here —
    /// mirrors [`TicketingError::EmptyAssigneeName`]'s rationale for
    /// rejecting an empty match target up front instead of sending it to the
    /// API.
    #[error("search text must not be empty")]
    EmptySearchText,

    /// `tm ticket unlink <KEY> <OTHER>` found no `Blocks`-type link between
    /// `key` and `other`, in either direction. Unlike [`open_blockers`],
    /// which only cares about `Blocks` links, `unlink_ticket` scans every
    /// link type so it can name any non-`Blocks` link it found between the
    /// pair (e.g. a `Relates` link) — a caller who expected to unlink a
    /// blocker relationship that never existed should learn what *does*
    /// exist between the two tickets instead of a bare "not found".
    #[error("no Blocks link between {key} and {other}{}", format_no_blocks_others(.others))]
    NoBlocksLinkBetween {
        /// The primary issue key passed to `unlink_ticket`.
        key: String,
        /// The other issue key passed to `unlink_ticket`.
        other: String,
        /// Semicolon-joined summary of non-`Blocks` links found between
        /// `key` and `other` (e.g. `"relates to PROJ-2"`), or an empty
        /// string when no link of any type exists between the pair. See
        /// [`format_no_blocks_others`].
        others: String,
    },

    /// `tm ticket comment` was given no explicit `KEY`, and no ticket key
    /// could be resolved either: either the current branch has no open pull
    /// request at all, or it does but [`resolve_existing_key`] found no key
    /// carried by it (title, body, or branch name). Mirrors
    /// [`TicketingError::NoPrForBranch`]'s shape (it also needs
    /// [`GhCli::current_branch`] to name the branch), but distinct from it:
    /// a PR existing with no derivable key is also this error, not
    /// `NoPrForBranch`, since the missing piece here is a *ticket*, not a
    /// PR.
    #[error(
        "no ticket key given and none could be resolved for branch `{branch}`; pass a key or run `tm ticket <KEY>` first"
    )]
    NoTicketOrPrForBranch {
        /// The branch a ticket key could not be resolved for.
        branch: String,
    },
}

/// Describe `others` for display in [`TicketingError::NoBlocksLinkBetween`]:
/// `"; other links exist: ..."` when non-`Blocks` links were found between
/// the pair, or an empty string (no suffix at all) when `others` is empty —
/// mirrors [`format_transitions`]'s "say so explicitly instead of a dangling
/// list" approach, just inverted (here, the *absence* of a suffix is the
/// common case, not an explicit "none" message).
fn format_no_blocks_others(others: &str) -> String {
    if others.is_empty() {
        String::new()
    } else {
        format!("; other links exist: {others}")
    }
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
    associate(ctx, key, &pr, None)
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
/// strip); its description is `pr.body` followed by the PR URL, handed to
/// [`TicketProvider::create_issue`] as plain Markdown.
pub fn auto_create_and_associate(
    ctx: &TicketingContext,
    pr: &PrInfo,
) -> Result<AssociateOutcome, TicketingError> {
    let req = NewTicket {
        project_key: ctx.config.default_project_key.clone(),
        summary: pr.title.clone(),
        description: format!("{}\n\n{}", pr.body, pr.url),
        issue_type_name: "Task".to_string(),
        assignee_account_id: ctx.config.default_assignee_account_id.clone(),
    };
    let issue = ctx.jira.create_issue(&req)?;
    let status_transition = ctx
        .config
        .status_on_pr
        .as_ref()
        .map(|target| apply_status_transition(ctx.jira, &issue.key, target));
    associate(ctx, &issue.key, pr, status_transition)
}

/// `tm pr create` with a key already carried by the PR (via title, body, or
/// branch name, resolved by [`resolve_existing_key`]): verify `key` exists,
/// associate it with the pull request open for the current branch, and,
/// unlike [`associate_ticket`], apply [`Config::status_on_pr`] to it.
///
/// This is a separate entry point from [`associate_ticket`] specifically so
/// that `tm ticket <KEY>` and `tm pr status`'s read-only report keep their
/// existing never-transitions semantics, while `tm pr create` gets the new
/// behavior. If the issue is already sitting in the target status
/// (case-insensitive match on its current status name), the transition is
/// skipped entirely and `status_transition` comes back `None` — there is
/// nothing to do and nothing to report.
///
/// Reuses the [`TicketProvider::get_issue`] call `associate_ticket` already made
/// on this path rather than issuing a second one, since that call's response
/// already carries the issue's current status.
pub fn associate_existing_ticket_for_pr_create(
    ctx: &TicketingContext,
    key: &str,
) -> Result<AssociateOutcome, TicketingError> {
    let issue = ctx.jira.get_issue(key)?;
    let pr = current_branch_pr(ctx)?;

    let status_transition = ctx.config.status_on_pr.as_ref().and_then(|target| {
        if issue.fields.status.name.eq_ignore_ascii_case(target) {
            None
        } else {
            Some(apply_status_transition(ctx.jira, key, target))
        }
    });

    associate(ctx, key, &pr, status_transition)
}

/// Outcome of successfully creating a ticket via [`create_ticket`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateTicketOutcome {
    /// The newly created issue's key.
    pub issue_key: String,
    /// Browsable URL of the newly created issue.
    pub issue_url: String,
    /// Outcome of attempting to move the ticket to the effective transition
    /// target passed to [`create_ticket`] (its `status_target` argument),
    /// if any. `None` when there was no target to transition to.
    pub status_transition: Option<StatusTransition>,
}

/// `tm ticket create`: create a new issue in the configured default project,
/// assigned to the configured default assignee, with no pull request
/// involved.
///
/// `body`, if given, is handed to [`TicketProvider::create_issue`] as plain
/// Markdown, which the provider (Jira today) converts into its own
/// description format internally; when absent, the issue is created with an
/// empty description. `status_target`, if given, is where the new ticket is moved
/// via [`apply_status_transition`] — the same case-insensitive matching used
/// by the `tm pr create` paths. Resolving whether that target comes from `tm
/// ticket create`'s `--status` flag, `--no-transition`, or falls back to
/// [`Config::status_on_create`] is the caller's job (see
/// [`crate::cli::ticket::create`]); this function only ever applies whatever
/// it's given.
pub fn create_ticket(
    ctx: &CreateTicketContext,
    title: &str,
    body: Option<&str>,
    status_target: Option<&str>,
) -> Result<CreateTicketOutcome, TicketingError> {
    let req = NewTicket {
        project_key: ctx.config.default_project_key.clone(),
        summary: title.to_string(),
        description: body.unwrap_or_default().to_string(),
        issue_type_name: "Task".to_string(),
        assignee_account_id: ctx.config.default_assignee_account_id.clone(),
    };
    let issue = ctx.jira.create_issue(&req)?;
    let status_transition =
        status_target.map(|target| apply_status_transition(ctx.jira, &issue.key, target));

    Ok(CreateTicketOutcome {
        issue_key: issue.key.clone(),
        issue_url: format!("{}/browse/{}", ctx.config.jira_base_url, issue.key),
        status_transition,
    })
}

/// Move an issue to `target`'s workflow status.
///
/// Fetches the issue's available transitions and picks the first one whose
/// target status name matches `target` case-insensitively, falling back to
/// matching the transition's own name case-insensitively if none of the
/// target statuses match (some workflows name a transition the same as the
/// status it leads to). Never propagates an error: any failure — no
/// matching transition, or the transition API call itself failing — is
/// reported as a [`StatusTransition::Warning`], since the ticket has
/// already been created (and, where applicable, linked) by this point.
fn apply_status_transition(jira: &dyn TicketProvider, key: &str, target: &str) -> StatusTransition {
    let transitions = match jira.transitions(key) {
        Ok(transitions) => transitions,
        Err(err) => {
            return StatusTransition::Warning(format!(
                "could not fetch transitions for {key}: {err}"
            ));
        }
    };

    let Some(transition) = find_matching_transition(&transitions, target) else {
        return StatusTransition::Warning(format!(
            "no transition to \"{target}\" found for {key}; leaving it in its initial status"
        ));
    };

    match jira.transition(key, &transition.id) {
        Ok(()) => StatusTransition::Applied(target.to_string()),
        Err(err) => {
            StatusTransition::Warning(format!("failed to transition {key} to \"{target}\": {err}"))
        }
    }
}

/// Find the transition (if any) among `transitions` that leads to `target`.
///
/// Picks the first transition whose target status name matches `target`
/// case-insensitively, falling back to matching the transition's own name
/// case-insensitively if none of the target statuses match (some workflows
/// name a transition the same as the status it leads to). Shared by
/// [`apply_status_transition`] (the advisory `status_on_pr`/`status_on_create`
/// paths) and [`transition_ticket`] (the explicit, hard-error `tm ticket
/// transition <KEY> <STATUS>` path) so both use identical matching rules.
fn find_matching_transition<'a>(
    transitions: &'a [crate::jira::types::Transition],
    target: &str,
) -> Option<&'a crate::jira::types::Transition> {
    transitions
        .iter()
        .find(|t| t.to.name.eq_ignore_ascii_case(target))
        .or_else(|| {
            transitions
                .iter()
                .find(|t| t.name.eq_ignore_ascii_case(target))
        })
}

/// Describe `transitions` for display in
/// [`TicketingError::NoMatchingTransition`]: `"available transitions: name
/// -> target status, ..."`, or, when the issue has none at all (common for
/// closed tickets), `"the ticket has no available transitions"` rather than
/// a dangling, empty list.
fn format_transitions(transitions: &[crate::jira::types::Transition]) -> String {
    if transitions.is_empty() {
        return "the ticket has no available transitions".to_string();
    }
    let list = transitions
        .iter()
        .map(|t| format!("{} -> {}", t.name, t.to.name))
        .collect::<Vec<_>>()
        .join(", ");
    format!("available transitions: {list}")
}

/// Outcome of [`transition_ticket`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransitionOutcome {
    /// The ticket was moved; carries the resolved target status name (the
    /// matched transition's actual `to` status, which may differ in case
    /// from the requested target).
    Applied(String),
    /// The ticket's current status already matched the requested target
    /// case-insensitively, so no transition was applied. Carries the
    /// ticket's actual current status name.
    AlreadyInStatus(String),
}

/// `tm ticket transition <KEY> <STATUS>`: move `key` to `target`'s workflow
/// status.
///
/// Unlike [`apply_status_transition`] (used by the advisory
/// `status_on_pr`/`status_on_create` paths, where a transition problem is
/// merely a warning since the ticket was already created/linked by the time
/// it's attempted), this command is explicit, so failure is a hard error:
/// [`TicketingError::NoMatchingTransition`] if no transition matches `target`
/// (by either rule in [`find_matching_transition`]), or the underlying
/// [`JiraError`] if fetching the issue, fetching its transitions, or
/// applying the transition fails.
///
/// If the issue's current status already equals `target` case-insensitively,
/// the transition is skipped entirely and
/// [`TransitionOutcome::AlreadyInStatus`] is returned (mirrors
/// [`associate_existing_ticket_for_pr_create`]'s skip-if-already-there
/// check).
pub fn transition_ticket(
    jira: &dyn TicketProvider,
    key: &str,
    target: &str,
) -> Result<TransitionOutcome, TicketingError> {
    let issue = jira.get_issue(key)?;
    if issue.fields.status.name.eq_ignore_ascii_case(target) {
        return Ok(TransitionOutcome::AlreadyInStatus(issue.fields.status.name));
    }

    let transitions = jira.transitions(key)?;
    let matching = find_matching_transition(&transitions, target).ok_or_else(|| {
        TicketingError::NoMatchingTransition {
            key: key.to_string(),
            target: target.to_string(),
            available: format_transitions(&transitions),
        }
    })?;
    let resolved_status = matching.to.name.clone();
    let transition_id = matching.id.clone();

    jira.transition(key, &transition_id)?;
    Ok(TransitionOutcome::Applied(resolved_status))
}

/// The current status and available workflow transitions of a ticket, as
/// returned by [`list_transitions`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionListing {
    /// The ticket's current status name.
    pub current_status: String,
    /// Transitions available on the ticket right now.
    pub transitions: Vec<crate::jira::types::Transition>,
}

/// `tm ticket transition <KEY>` (no status): list `key`'s current status and
/// available workflow transitions, for a caller to choose a target status
/// with [`transition_ticket`].
pub fn list_transitions(
    jira: &dyn TicketProvider,
    key: &str,
) -> Result<TransitionListing, TicketingError> {
    let issue = jira.get_issue(key)?;
    let transitions = jira.transitions(key)?;
    Ok(TransitionListing {
        current_status: issue.fields.status.name,
        transitions,
    })
}

/// What `tm ticket assign <KEY> ...` was asked to do, mirroring its
/// mutually-exclusive CLI flags (`NAME`, `--me`, `--unassign`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssignTarget {
    /// Resolve the given name against the issue's project's assignable
    /// users and assign to the match.
    Name(String),
    /// Assign to the current user.
    Me,
    /// Clear the issue's assignee.
    Unassign,
}

/// Outcome of successfully assigning (or unassigning) a ticket via
/// [`assign_ticket`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssignOutcome {
    /// Assigned by name; carries the resolved user's displayName.
    AssignedToUser(String),
    /// Assigned to the current user; carries a display label for the CLI to
    /// print. This is [`crate::jira::types::Myself::display_name`] when
    /// [`Config::default_assignee_account_id`] wasn't cached and `myself()`
    /// had to be called, or the cached account ID itself when it was
    /// (fetching `myself()` just to get a display name isn't worth the extra
    /// request when the cached ID already lets the assign call proceed).
    AssignedToMe(String),
    /// The issue's assignee was cleared.
    Unassigned,
}

/// `tm ticket assign <KEY> ...`: assign `key` by resolved name, to the
/// current user, or clear its assignee, per `target`.
///
/// Like [`transition_ticket`], this command is explicit, so every failure
/// mode is a hard error: an ambiguous or unknown name is
/// [`TicketingError::NoMatchingAssignee`], and any Jira API failure
/// propagates as the underlying [`JiraError`].
///
/// [`AssignTarget::Name`] resolves against the assignable users of `key`'s
/// *own* project (derived from `key`'s prefix via
/// [`project_key_from_issue_key`]), not [`Config::default_project_key`], so
/// assigning a ticket that lives in a different project still works.
/// [`AssignTarget::Me`] prefers `config`'s cached
/// [`Config::default_assignee_account_id`] (set by `tm auth login`) over an
/// extra `myself()` call.
pub fn assign_ticket(
    jira: &dyn TicketProvider,
    config: &Config,
    key: &str,
    target: &AssignTarget,
) -> Result<AssignOutcome, TicketingError> {
    match target {
        AssignTarget::Unassign => {
            jira.assign(key, None)?;
            Ok(AssignOutcome::Unassigned)
        }
        AssignTarget::Me => {
            let (account_id, label) = match &config.default_assignee_account_id {
                Some(account_id) => (account_id.clone(), account_id.clone()),
                None => {
                    let myself = jira.myself()?;
                    (myself.account_id, myself.display_name)
                }
            };
            jira.assign(key, Some(&account_id))?;
            Ok(AssignOutcome::AssignedToMe(label))
        }
        AssignTarget::Name(name) => {
            if name.trim().is_empty() {
                return Err(TicketingError::EmptyAssigneeName {
                    key: key.to_string(),
                });
            }
            let project = project_key_from_issue_key(key);
            let users = jira.assignable_users(project)?;
            let user = resolve_assignee_by_name(&users, name, key)?;
            let account_id = user.account_id.clone();
            let display_name = user.display_name.clone();
            jira.assign(key, Some(&account_id))?;
            Ok(AssignOutcome::AssignedToUser(display_name))
        }
    }
}

/// `tm ticket rank <KEY> (--above|--below) <OTHER>`: move `key` to a new
/// position in the backlog rank, relative to `anchor`.
///
/// Verifies `key` exists first (via [`TicketProvider::get_issue`]) so a typo'd
/// primary key gives the same friendly [`JiraError::NotFound`] every other
/// `tm ticket` subcommand does, rather than surfacing as a raw
/// [`JiraError::RankNotFound`] from the agile API. A typo'd anchor key (in
/// `anchor`) is not checked ahead of time; it surfaces from the `rank` call
/// itself as [`JiraError::RankNotFound`], since Jira's rank endpoint reports
/// that case directly and a second lookup would be redundant.
pub fn rank_ticket(
    jira: &dyn TicketProvider,
    key: &str,
    anchor: RankAnchor,
) -> Result<(), TicketingError> {
    jira.get_issue(key)?;
    jira.rank(&[key.to_string()], anchor)?;
    Ok(())
}

/// `tm ticket link <KEY> (--blocks|--blocked-by) <OTHER>`: create a `Blocks`
/// link between two tickets, per `req` (already resolved to the correct
/// `blocker_key`/`blocked_key` direction by the CLI layer).
///
/// Verifies `key` (the primary ticket named on the command line, not
/// necessarily `req.blocker_key`) exists first, for the same reason
/// [`rank_ticket`] does: a typo there gets the friendly
/// [`JiraError::NotFound`] every other `tm ticket` subcommand gives, rather
/// than surfacing as a raw [`JiraError::LinkNotFound`] from the link API. A
/// typo'd `OTHER` is not checked ahead of time; it surfaces from the
/// `create_link` call itself as [`JiraError::LinkNotFound`].
pub fn link_ticket(
    jira: &dyn TicketProvider,
    key: &str,
    req: &CreateLinkRequest,
) -> Result<(), TicketingError> {
    jira.get_issue(key)?;
    jira.create_link(req)?;
    Ok(())
}

/// Result of successfully [`unlink_ticket`]ing two tickets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnlinkOutcome {
    /// The direction phrase of each removed link (e.g. `"blocks"`, `"is
    /// blocked by"`), relative to `key`, in encounter order. Normally a
    /// single entry; two when both an inward and an outward `Blocks` link
    /// existed between the pair (degenerate but possible in Jira).
    pub removed: Vec<String>,
}

/// `tm ticket unlink <KEY> <OTHER>`: remove the `Blocks`-type link(s) between
/// `key` and `other`, regardless of direction.
///
/// Fetches `key` and scans its `issuelinks` for entries where
/// [`IssueLinkType::name`](crate::jira::types::IssueLinkType) is `Blocks` and
/// the other side (`inward_issue` or `outward_issue`) is `other`; each match
/// is deleted by its link id via [`TicketProvider::delete_link`]. No match is a
/// hard error ([`TicketingError::NoBlocksLinkBetween`]), naming any
/// non-`Blocks` links found between the pair so a caller who mistyped the
/// relationship type learns what does exist instead of a bare "not found". A
/// `Blocks` link to a different issue entirely is left untouched.
pub fn unlink_ticket(
    jira: &dyn TicketProvider,
    key: &str,
    other: &str,
) -> Result<UnlinkOutcome, TicketingError> {
    let issue = jira.get_issue(key)?;

    let mut to_delete: Vec<(String, String)> = Vec::new();
    let mut other_links: Vec<String> = Vec::new();

    for link in &issue.fields.issue_links {
        let side = if link.inward_issue.as_ref().is_some_and(|i| i.key == other) {
            Some(link.link_type.inward.clone())
        } else if link.outward_issue.as_ref().is_some_and(|o| o.key == other) {
            Some(link.link_type.outward.clone())
        } else {
            None
        };
        let Some(phrase) = side else { continue };

        if link.link_type.name == "Blocks" {
            to_delete.push((link.id.clone(), phrase));
        } else {
            other_links.push(format!("{phrase} {other}"));
        }
    }

    if to_delete.is_empty() {
        return Err(TicketingError::NoBlocksLinkBetween {
            key: key.to_string(),
            other: other.to_string(),
            others: other_links.join("; "),
        });
    }

    let mut removed = Vec::new();
    for (link_id, phrase) in to_delete {
        jira.delete_link(&link_id)?;
        removed.push(phrase);
    }
    Ok(UnlinkOutcome { removed })
}

/// A ticket's existing issue links, as returned by [`list_links`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkListing {
    /// Every issue-link entry attached to the ticket, of any link type.
    pub links: Vec<IssueLink>,
}

/// `tm ticket link <KEY>` (no flag): list `key`'s existing issue links, of
/// any link type, for discovery. Unlike [`link_ticket`], this never creates
/// anything.
pub fn list_links(jira: &dyn TicketProvider, key: &str) -> Result<LinkListing, TicketingError> {
    let issue = jira.get_issue(key)?;
    Ok(LinkListing {
        links: issue.fields.issue_links,
    })
}

/// The open (not-Done) `Blocks`-type blockers of `issue`: the linked issues
/// named by an inward `Blocks` entry (`inward_issue: Some(Y)` means "`issue`
/// is blocked by `Y`", see [`IssueLink`]'s doc comment) whose status category
/// isn't `done`. A Done blocker doesn't block; an outward `Blocks` entry
/// (`issue` blocks someone else) never makes `issue` itself blocked; a link
/// of any other type (e.g. `Relates`) is ignored regardless of direction.
///
/// Pure and unit-testable: used by both [`ready_tickets`] (to filter search
/// results) and [`check_ready`] (to report a single ticket's blockers).
pub fn open_blockers(issue: &Issue) -> Vec<&LinkedIssue> {
    issue
        .fields
        .issue_links
        .iter()
        .filter(|link| link.link_type.name == "Blocks")
        .filter_map(|link| link.inward_issue.as_ref())
        .filter(|blocker| blocker.fields.status.status_category.key != "done")
        .collect()
}

/// Result of [`ready_tickets`]: the caller's ready-to-pick-up tickets, in the
/// same rank order the search returned, plus the candidates that were
/// excluded for having an open blocker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyListing {
    /// Tickets assigned to the current user, in "To Do", with no open
    /// blockers, in rank order.
    pub ready: Vec<Issue>,
    /// Candidates excluded because [`open_blockers`] found at least one, in
    /// rank order. `tm ready`'s CLI layer (`crate::cli::ready`) re-examines
    /// each of these against `crate::blocker_stacking` to tell a genuinely
    /// stuck ticket apart from a stackable one — this module stays Jira-only
    /// and doesn't make that call itself.
    pub blocked: Vec<Issue>,
}

impl ReadyListing {
    /// Number of candidates excluded because [`open_blockers`] found at
    /// least one. Surfaced by the CLI so a caller doesn't mistake a filtered
    /// list for the complete set of assigned tickets.
    pub fn hidden_blocked_count(&self) -> usize {
        self.blocked.len()
    }
}

/// `tm ready` (no key): search the current user's "To Do" tickets (via
/// [`TicketQuery::ReadyCandidates`]) and keep only those with no open blockers.
///
/// Rank order from the search is preserved in both [`ReadyListing::ready`]
/// and [`ReadyListing::blocked`].
pub fn ready_tickets(jira: &dyn TicketProvider) -> Result<ReadyListing, TicketingError> {
    let result = jira.search(&TicketQuery::ReadyCandidates)?;
    let mut ready = Vec::new();
    let mut blocked = Vec::new();
    for issue in result.issues {
        if open_blockers(&issue).is_empty() {
            ready.push(issue);
        } else {
            blocked.push(issue);
        }
    }
    Ok(ReadyListing { ready, blocked })
}

/// Result of [`check_ready`]: `key`'s current status and its open blockers,
/// if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyCheck {
    /// `key`'s current workflow status name.
    pub status_name: String,
    /// `key`'s open (not-Done) `Blocks`-type blockers, per [`open_blockers`].
    /// Empty means `key` is ready to pick up.
    pub open_blockers: Vec<LinkedIssue>,
}

/// `tm ready <KEY>`: fetch `key` (any assignee, any status) and report
/// whether it has any open blockers.
pub fn check_ready(jira: &dyn TicketProvider, key: &str) -> Result<ReadyCheck, TicketingError> {
    let issue = jira.get_issue(key)?;
    let status_name = issue.fields.status.name.clone();
    let open_blockers = open_blockers(&issue).into_iter().cloned().collect();
    Ok(ReadyCheck {
        status_name,
        open_blockers,
    })
}

/// `tm ticket search <TEXT>`: search [`Config::default_project_key`] for
/// open tickets whose text matches `text`, most recently updated first (via
/// [`TicketQuery::Search`]).
///
/// Fails with [`TicketingError::EmptySearchText`] if `text` is empty or
/// all-whitespace, mirroring [`assign_ticket`]'s [`TicketingError::EmptyAssigneeName`]
/// check: an empty search is never a meaningful request, and Jira's `text ~
/// ""` behavior for it isn't worth relying on.
pub fn search_tickets(
    jira: &dyn TicketProvider,
    config: &Config,
    text: &str,
) -> Result<Vec<Issue>, TicketingError> {
    if text.trim().is_empty() {
        return Err(TicketingError::EmptySearchText);
    }
    let query = TicketQuery::Search {
        project_key: config.default_project_key.clone(),
        text: text.to_string(),
    };
    let result = jira.search(&query)?;
    Ok(result.issues)
}

/// Outcome of successfully [`comment_ticket`]ing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentOutcome {
    /// The issue key the Jira comment was posted to. Always `Some` on a
    /// successful [`comment_ticket`] call — a missing key is
    /// [`TicketingError::NoTicketOrPrForBranch`] instead — but kept as an
    /// `Option` alongside [`CommentOutcome::pr_number`] for a symmetric
    /// shape.
    pub issue_key: Option<String>,
    /// The pull request number a copy of the comment was also posted to, if
    /// `also_pr` was requested. `None` when it wasn't.
    pub pr_number: Option<u64>,
}

/// `tm ticket comment [<KEY>] [--body <TEXT>] [--pr]`: post a comment to a
/// Jira ticket, optionally also to the current branch's pull request.
///
/// `body_markdown` is GitHub-flavored Markdown, handed to
/// [`TicketProvider::add_comment`] as-is; the provider (Jira today) converts
/// it internally for the Jira comment, but it's posted to the pull request
/// as-is when `also_pr` is set — GitHub comments are Markdown natively, so
/// no conversion happens on that path (unlike Jira's v3 API, which only
/// accepts ADF).
///
/// `key`, if given, is validated with [`TicketProvider::get_issue`] first (like
/// [`rank_ticket`]/[`link_ticket`]) so a typo'd key gives the familiar
/// [`JiraError::NotFound`] rather than a raw comment-post 404. If `key` is
/// omitted, it's resolved from the current branch's pull request the same
/// way [`resolve_existing_key`] does for `tm pr create` — this is the only
/// way to derive a key without one being given explicitly. If neither an
/// explicit key nor a resolvable one is available (no pull request for the
/// branch at all, or one exists but carries no key), this fails with
/// [`TicketingError::NoTicketOrPrForBranch`] rather than silently doing
/// nothing.
///
/// `--pr` means the pull request open for the **current branch**, not "the
/// pull request associated with the ticket" — there is no reverse
/// issue-to-PR lookup in this codebase (it would require re-deriving one via
/// `gh pr list`), and every other explicit `tm ticket <KEY>` command already
/// only ever touches the current branch's PR. When `also_pr` is set and `key`
/// was given explicitly (so no pull request has been looked up yet), the
/// current branch's PR is fetched via [`GhCli::pr_view`], failing with
/// [`TicketingError::NoPrForBranch`] if there is none.
///
/// Like every other explicit `tm ticket` subcommand (`transition`, `assign`,
/// `rank`, `link`, `unlink`, `update`), every failure here is a hard error:
/// there is no advisory/warning path analogous to
/// [`apply_status_transition`]'s, since nothing has already been created or
/// linked by the time a comment attempt fails.
pub fn comment_ticket(
    ctx: &TicketingContext,
    key: Option<&str>,
    body_markdown: &str,
    also_pr: bool,
) -> Result<CommentOutcome, TicketingError> {
    let (issue_key, pr): (Option<String>, Option<PrInfo>) = match key {
        Some(key) => {
            ctx.jira.get_issue(key)?;
            (Some(key.to_string()), None)
        }
        None => match ctx.gh.pr_view()? {
            Some(pr) => {
                let issue_key = resolve_existing_key(ctx.jira, &pr)?;
                (issue_key, Some(pr))
            }
            None => (None, None),
        },
    };

    let Some(issue_key) = issue_key else {
        let branch = ctx.gh.current_branch()?;
        return Err(TicketingError::NoTicketOrPrForBranch { branch });
    };

    ctx.jira.add_comment(&issue_key, body_markdown)?;

    let pr_number = if also_pr {
        let pr = match pr {
            Some(pr) => pr,
            None => current_branch_pr(ctx)?,
        };
        ctx.gh.pr_comment(pr.number, body_markdown)?;
        Some(pr.number)
    } else {
        None
    };

    Ok(CommentOutcome {
        issue_key: Some(issue_key),
        pr_number,
    })
}

/// Extract the project key prefix from an issue key, e.g. `PROJ` from
/// `PROJ-372`.
///
/// Issue keys are validated by the CLI layer's `normalize_key`
/// (`^[A-Z][A-Z0-9]+-\d+$`) before reaching here, so the project part never
/// itself contains a `-`; falls back to the whole key in the (should-not-happen)
/// case of no `-` at all.
fn project_key_from_issue_key(key: &str) -> &str {
    key.split_once('-').map_or(key, |(project, _)| project)
}

/// Resolve `name` against `users`: a case-insensitive exact match on
/// `display_name` wins first, but only if it is unambiguous (exactly one
/// user has that exact name — real Jira projects can have two distinct
/// accounts sharing a `displayName`, so this is not assumed away); failing
/// that, a case-insensitive substring match, again only if it is unambiguous.
///
/// Fails with [`TicketingError::NoMatchingAssignee`] when either the exact or
/// the substring match finds zero or more than one candidate, listing the
/// candidates found (or, when none matched at all, every assignable user in
/// the project) via [`format_assignee_candidates`].
fn resolve_assignee_by_name<'a>(
    users: &'a [JiraUser],
    name: &str,
    key: &str,
) -> Result<&'a JiraUser, TicketingError> {
    let needle = name.to_lowercase();

    let exact_matches: Vec<&JiraUser> = users
        .iter()
        .filter(|u| u.display_name.to_lowercase() == needle)
        .collect();
    match exact_matches.as_slice() {
        [only] => return Ok(only),
        [] => {}
        _ => {
            return Err(TicketingError::NoMatchingAssignee {
                key: key.to_string(),
                name: name.to_string(),
                available: format_assignee_candidates(&exact_matches, users),
            });
        }
    }

    let candidates: Vec<&JiraUser> = users
        .iter()
        .filter(|u| u.display_name.to_lowercase().contains(&needle))
        .collect();

    match candidates.as_slice() {
        [only] => Ok(only),
        _ => Err(TicketingError::NoMatchingAssignee {
            key: key.to_string(),
            name: name.to_string(),
            available: format_assignee_candidates(&candidates, users),
        }),
    }
}

/// Describe assignee candidates for display in
/// [`TicketingError::NoMatchingAssignee`]: `"candidates: name (accountId),
/// ..."` when `candidates` is non-empty (an ambiguous exact or substring
/// match — account IDs are included so users sharing an identical
/// `displayName` are still distinguishable), or, when it's empty (no match
/// at all), every assignable user in the project the same way
/// (`"assignable users: name (accountId), ..."`), or, when there are none of
/// those either, says so explicitly rather than leaving a dangling empty
/// list.
fn format_assignee_candidates(candidates: &[&JiraUser], all_users: &[JiraUser]) -> String {
    if !candidates.is_empty() {
        let list = candidates
            .iter()
            .map(|u| format!("{} ({})", u.display_name, u.account_id))
            .collect::<Vec<_>>()
            .join(", ");
        return format!("candidates: {list}");
    }

    if all_users.is_empty() {
        return "no assignable users found in the project".to_string();
    }

    let list = all_users
        .iter()
        .map(|u| format!("{} ({})", u.display_name, u.account_id))
        .collect::<Vec<_>>()
        .join(", ");
    format!("assignable users: {list}")
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
///   authored, so it is validated with [`TicketProvider::get_issue`] first.
///   [`JiraError::NotFound`] is treated as "no key after all" (`Ok(None)`);
///   any other Jira error propagates, since it means the check itself
///   couldn't be completed.
///
/// Returns `Ok(None)` when no key is found by any means.
pub fn resolve_existing_key(
    jira: &dyn TicketProvider,
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
///
/// `status_transition` is passed through verbatim to the returned outcome;
/// only [`auto_create_and_associate`] ever computes a non-`None` value.
fn associate(
    ctx: &TicketingContext,
    key: &str,
    pr: &PrInfo,
    status_transition: Option<StatusTransition>,
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
        status_transition,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::gh_cli::FakeGhCli;
    use crate::jira::fake::FakeJiraClient;
    use crate::jira::types::{
        Issue, IssueFields, JiraUser, Myself, Status, StatusCategory, Transition,
    };

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

    fn pr(title: &str) -> PrInfo {
        PrInfo {
            number: 42,
            url: "https://github.com/example/repo/pull/42".to_string(),
            title: title.to_string(),
            body: String::new(),
            head_ref_name: "proj-372-fix".to_string(),
        }
    }

    fn config() -> Config {
        Config {
            backend: crate::config::BackendKind::Jira,
            jira_base_url: "https://example.atlassian.net".to_string(),
            jira_email: "ada@example.com".to_string(),
            default_project_key: "PROJ".to_string(),
            default_assignee_account_id: Some("acct-1".to_string()),
            status_on_pr: None,
            status_on_create: None,
            run_db_path: None,
            review_bots: vec!["cursor[bot]".to_string()],
            board_column_order: Vec::new(),
            work: crate::config::WorkConfig::default(),
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
        assert_eq!(outcome.status_transition, None);

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
            .with_current_branch(Ok("proj-372-fix".to_string()));
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };

        let err = associate_ticket(&ctx, "PROJ-1").expect_err("should fail");

        match err {
            TicketingError::NoPrForBranch { branch } => assert_eq!(branch, "proj-372-fix"),
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
        assert_eq!(outcome.status_transition, None);
        assert!(
            jira.transition_calls().is_empty(),
            "no status_on_pr configured, so transition should never be called"
        );
    }

    fn transition(id: &str, name: &str, to_status: &str) -> Transition {
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

    fn config_with_status_on_pr(status: &str) -> Config {
        Config {
            status_on_pr: Some(status.to_string()),
            ..config()
        }
    }

    #[test]
    fn auto_create_and_associate_applies_matching_transition_case_insensitively() {
        let jira = FakeJiraClient::new()
            .with_create_issue_result(issue("PROJ-9"))
            .with_transitions(
                "PROJ-9",
                vec![
                    transition("11", "Start Progress", "In Progress"),
                    transition("21", "Send to review", "in review"),
                ],
            );
        let gh = FakeGhCli::new();
        let cfg = config_with_status_on_pr("In Review");
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };

        let outcome =
            auto_create_and_associate(&ctx, &pr("Add the widget")).expect("should succeed");

        assert_eq!(
            outcome.status_transition,
            Some(StatusTransition::Applied("In Review".to_string()))
        );
        assert_eq!(
            jira.transition_calls(),
            vec![("PROJ-9".to_string(), "21".to_string())]
        );
    }

    #[test]
    fn auto_create_and_associate_falls_back_to_transition_name_match() {
        let jira = FakeJiraClient::new()
            .with_create_issue_result(issue("PROJ-9"))
            .with_transitions(
                "PROJ-9",
                vec![transition("31", "in review", "Under Review")],
            );
        let gh = FakeGhCli::new();
        let cfg = config_with_status_on_pr("In Review");
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };

        let outcome =
            auto_create_and_associate(&ctx, &pr("Add the widget")).expect("should succeed");

        assert_eq!(
            outcome.status_transition,
            Some(StatusTransition::Applied("In Review".to_string()))
        );
        assert_eq!(
            jira.transition_calls(),
            vec![("PROJ-9".to_string(), "31".to_string())]
        );
    }

    #[test]
    fn auto_create_and_associate_no_matching_transition_yields_warning_without_erroring() {
        let jira = FakeJiraClient::new()
            .with_create_issue_result(issue("PROJ-9"))
            .with_transitions(
                "PROJ-9",
                vec![transition("11", "Start Progress", "In Progress")],
            );
        let gh = FakeGhCli::new();
        let cfg = config_with_status_on_pr("In Review");
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };

        let outcome = auto_create_and_associate(&ctx, &pr("Add the widget"))
            .expect("should succeed even with no matching transition");

        match outcome.status_transition {
            Some(StatusTransition::Warning(msg)) => {
                assert!(
                    msg.contains("In Review"),
                    "warning should name the target: {msg}"
                )
            }
            other => panic!("expected Warning, got {other:?}"),
        }
        assert!(jira.transition_calls().is_empty());
        // The ticket was still created and linked.
        assert_eq!(outcome.issue_key, "PROJ-9");
        assert_eq!(jira.add_remote_link_calls().len(), 1);
    }

    #[test]
    fn auto_create_and_associate_transition_api_error_yields_warning_without_erroring() {
        let jira = FakeJiraClient::new()
            .with_create_issue_result(issue("PROJ-9"))
            .with_transitions(
                "PROJ-9",
                vec![transition("21", "Send to review", "In Review")],
            )
            .with_transition_error(500, "boom");
        let gh = FakeGhCli::new();
        let cfg = config_with_status_on_pr("In Review");
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };

        let outcome = auto_create_and_associate(&ctx, &pr("Add the widget"))
            .expect("should succeed even when the transition API call fails");

        match outcome.status_transition {
            Some(StatusTransition::Warning(msg)) => {
                assert!(
                    msg.contains("boom"),
                    "warning should surface the error: {msg}"
                )
            }
            other => panic!("expected Warning, got {other:?}"),
        }
        assert_eq!(outcome.issue_key, "PROJ-9");
    }

    #[test]
    fn auto_create_and_associate_transitions_fetch_error_yields_warning_without_erroring() {
        let jira = FakeJiraClient::new()
            .with_create_issue_result(issue("PROJ-9"))
            .with_transitions_error("PROJ-9", 500, "fetch boom");
        let gh = FakeGhCli::new();
        let cfg = config_with_status_on_pr("In Review");
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };

        let outcome = auto_create_and_associate(&ctx, &pr("Add the widget"))
            .expect("should succeed even when fetching transitions fails");

        match outcome.status_transition {
            Some(StatusTransition::Warning(msg)) => assert!(msg.contains("fetch boom")),
            other => panic!("expected Warning, got {other:?}"),
        }
        assert!(jira.transition_calls().is_empty());
    }

    #[test]
    fn associate_ticket_never_transitions_even_when_status_on_pr_configured() {
        let jira = FakeJiraClient::new()
            .with_issue("PROJ-1", issue("PROJ-1"))
            .with_transitions(
                "PROJ-1",
                vec![transition("21", "Send to review", "In Review")],
            );
        let gh = FakeGhCli::new().with_pr_view(Ok(Some(pr("Fix the thing"))));
        let cfg = config_with_status_on_pr("In Review");
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };

        let outcome = associate_ticket(&ctx, "PROJ-1").expect("should succeed");

        assert_eq!(outcome.status_transition, None);
        assert!(
            jira.transition_calls().is_empty(),
            "associate_ticket must never transition an existing ticket"
        );
    }

    #[test]
    fn associate_existing_ticket_for_pr_create_applies_matching_transition() {
        let jira = FakeJiraClient::new()
            .with_issue("PROJ-1", issue("PROJ-1"))
            .with_transitions(
                "PROJ-1",
                vec![transition("21", "Send to review", "In Review")],
            );
        let gh = FakeGhCli::new().with_pr_view(Ok(Some(pr("Fix the thing"))));
        let cfg = config_with_status_on_pr("In Review");
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };

        let outcome =
            associate_existing_ticket_for_pr_create(&ctx, "PROJ-1").expect("should succeed");

        assert_eq!(
            outcome.status_transition,
            Some(StatusTransition::Applied("In Review".to_string()))
        );
        assert_eq!(
            jira.transition_calls(),
            vec![("PROJ-1".to_string(), "21".to_string())]
        );
        assert_eq!(jira.add_remote_link_calls().len(), 1);
    }

    #[test]
    fn associate_existing_ticket_for_pr_create_skips_transition_when_already_in_target_status() {
        let mut already_in_review = issue("PROJ-1");
        already_in_review.fields.status.name = "In Review".to_string();
        let jira = FakeJiraClient::new()
            .with_issue("PROJ-1", already_in_review)
            .with_transitions(
                "PROJ-1",
                vec![transition("21", "Send to review", "In Review")],
            );
        let gh = FakeGhCli::new().with_pr_view(Ok(Some(pr("Fix the thing"))));
        // Case-insensitive match against the ticket's current status.
        let cfg = config_with_status_on_pr("in review");
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };

        let outcome =
            associate_existing_ticket_for_pr_create(&ctx, "PROJ-1").expect("should succeed");

        assert_eq!(outcome.status_transition, None);
        assert!(
            jira.transition_calls().is_empty(),
            "should not call transition when the issue is already in the target status"
        );
        // Association itself still happens.
        assert_eq!(jira.add_remote_link_calls().len(), 1);
    }

    #[test]
    fn associate_existing_ticket_for_pr_create_no_status_on_pr_never_transitions() {
        let jira = FakeJiraClient::new().with_issue("PROJ-1", issue("PROJ-1"));
        let gh = FakeGhCli::new().with_pr_view(Ok(Some(pr("Fix the thing"))));
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };

        let outcome =
            associate_existing_ticket_for_pr_create(&ctx, "PROJ-1").expect("should succeed");

        assert_eq!(outcome.status_transition, None);
        assert!(jira.transition_calls().is_empty());
    }

    fn create_ctx<'a>(jira: &'a FakeJiraClient, cfg: &'a Config) -> CreateTicketContext<'a> {
        CreateTicketContext { jira, config: cfg }
    }

    #[test]
    fn create_ticket_creates_issue_with_expected_fields() {
        let jira = FakeJiraClient::new().with_create_issue_result(issue("PROJ-9"));
        let cfg = config();
        let ctx = create_ctx(&jira, &cfg);

        let outcome = create_ticket(&ctx, "Add the widget", Some("Some **body**"), None)
            .expect("should succeed");

        let calls = jira.create_issue_calls();
        assert_eq!(calls.len(), 1);
        let req = &calls[0];
        assert_eq!(req.project_key, "PROJ");
        assert_eq!(req.summary, "Add the widget");
        assert_eq!(req.issue_type_name, "Task");
        assert_eq!(req.assignee_account_id, Some("acct-1".to_string()));
        let description = req.description.to_string();
        assert!(
            description.contains("\"strong\""),
            "markdown body should be converted to ADF marks: {description}"
        );

        assert_eq!(outcome.issue_key, "PROJ-9");
        assert_eq!(
            outcome.issue_url,
            "https://example.atlassian.net/browse/PROJ-9"
        );
        assert_eq!(outcome.status_transition, None);
    }

    #[test]
    fn create_ticket_without_body_has_empty_description() {
        let jira = FakeJiraClient::new().with_create_issue_result(issue("PROJ-9"));
        let cfg = config();
        let ctx = create_ctx(&jira, &cfg);

        create_ticket(&ctx, "Add the widget", None, None).expect("should succeed");

        let calls = jira.create_issue_calls();
        assert_eq!(
            calls[0].description,
            serde_json::json!({ "type": "doc", "version": 1, "content": [] })
        );
    }

    #[test]
    fn create_ticket_applies_given_status_target_transition() {
        let jira = FakeJiraClient::new()
            .with_create_issue_result(issue("PROJ-9"))
            .with_transitions(
                "PROJ-9",
                vec![transition("11", "Start Progress", "In Progress")],
            );
        let cfg = config();
        let ctx = create_ctx(&jira, &cfg);

        let outcome = create_ticket(&ctx, "Add the widget", None, Some("In Progress"))
            .expect("should succeed");

        assert_eq!(
            outcome.status_transition,
            Some(StatusTransition::Applied("In Progress".to_string()))
        );
        assert_eq!(
            jira.transition_calls(),
            vec![("PROJ-9".to_string(), "11".to_string())]
        );
    }

    #[test]
    fn create_ticket_no_matching_transition_yields_warning_without_erroring() {
        let jira = FakeJiraClient::new()
            .with_create_issue_result(issue("PROJ-9"))
            .with_transitions("PROJ-9", vec![]);
        let cfg = config();
        let ctx = create_ctx(&jira, &cfg);

        let outcome = create_ticket(&ctx, "Add the widget", None, Some("In Progress"))
            .expect("should succeed even with no matching transition");

        match outcome.status_transition {
            Some(StatusTransition::Warning(msg)) => {
                assert!(
                    msg.contains("In Progress"),
                    "warning should name the target: {msg}"
                )
            }
            other => panic!("expected Warning, got {other:?}"),
        }
    }

    #[test]
    fn create_ticket_no_status_target_never_transitions() {
        let jira = FakeJiraClient::new().with_create_issue_result(issue("PROJ-9"));
        let cfg = config();
        let ctx = create_ctx(&jira, &cfg);

        let outcome = create_ticket(&ctx, "Add the widget", None, None).expect("should succeed");

        assert_eq!(outcome.status_transition, None);
        assert!(jira.transition_calls().is_empty());
    }

    #[test]
    fn transition_ticket_applies_matching_transition_case_insensitively() {
        let jira = FakeJiraClient::new()
            .with_issue("PROJ-1", issue("PROJ-1"))
            .with_transitions(
                "PROJ-1",
                vec![
                    transition("11", "Start Progress", "In Progress"),
                    transition("21", "Send to review", "in review"),
                ],
            );

        let outcome = transition_ticket(&jira, "PROJ-1", "In Review").expect("should succeed");

        assert_eq!(outcome, TransitionOutcome::Applied("in review".to_string()));
        assert_eq!(
            jira.transition_calls(),
            vec![("PROJ-1".to_string(), "21".to_string())]
        );
    }

    #[test]
    fn transition_ticket_falls_back_to_transition_name_match() {
        let jira = FakeJiraClient::new()
            .with_issue("PROJ-1", issue("PROJ-1"))
            .with_transitions(
                "PROJ-1",
                vec![transition("31", "in review", "Under Review")],
            );

        let outcome = transition_ticket(&jira, "PROJ-1", "In Review").expect("should succeed");

        assert_eq!(
            outcome,
            TransitionOutcome::Applied("Under Review".to_string())
        );
        assert_eq!(
            jira.transition_calls(),
            vec![("PROJ-1".to_string(), "31".to_string())]
        );
    }

    #[test]
    fn transition_ticket_skips_when_already_in_target_status() {
        let mut already_in_review = issue("PROJ-1");
        already_in_review.fields.status.name = "In Review".to_string();
        let jira = FakeJiraClient::new()
            .with_issue("PROJ-1", already_in_review)
            .with_transitions(
                "PROJ-1",
                vec![transition("21", "Send to review", "In Review")],
            );

        // Case-insensitive match against the ticket's current status.
        let outcome = transition_ticket(&jira, "PROJ-1", "in review").expect("should succeed");

        assert_eq!(
            outcome,
            TransitionOutcome::AlreadyInStatus("In Review".to_string())
        );
        assert!(jira.transition_calls().is_empty());
    }

    #[test]
    fn transition_ticket_no_matching_transition_is_a_hard_error_listing_available() {
        let jira = FakeJiraClient::new()
            .with_issue("PROJ-1", issue("PROJ-1"))
            .with_transitions(
                "PROJ-1",
                vec![transition("11", "Start Progress", "In Progress")],
            );

        let err = transition_ticket(&jira, "PROJ-1", "In Review").expect_err("should fail");

        match err {
            TicketingError::NoMatchingTransition {
                key,
                target,
                available,
            } => {
                assert_eq!(key, "PROJ-1");
                assert_eq!(target, "In Review");
                assert!(
                    available.contains("Start Progress") && available.contains("In Progress"),
                    "available should list transition name and target status: {available}"
                );
            }
            other => panic!("expected NoMatchingTransition, got {other:?}"),
        }
        assert!(jira.transition_calls().is_empty());
    }

    #[test]
    fn transition_ticket_no_transitions_available_is_a_hard_error_with_sensible_message() {
        let jira = FakeJiraClient::new()
            .with_issue("PROJ-1", issue("PROJ-1"))
            .with_transitions("PROJ-1", vec![]);

        let err = transition_ticket(&jira, "PROJ-1", "In Review").expect_err("should fail");

        match err {
            TicketingError::NoMatchingTransition {
                key,
                target,
                available,
            } => {
                assert_eq!(key, "PROJ-1");
                assert_eq!(target, "In Review");
                assert_eq!(available, "the ticket has no available transitions");
            }
            other => panic!("expected NoMatchingTransition, got {other:?}"),
        }
    }

    #[test]
    fn transition_ticket_propagates_get_issue_error() {
        let jira = FakeJiraClient::new().with_issue_not_found("PROJ-404");

        let err = transition_ticket(&jira, "PROJ-404", "In Review").expect_err("should fail");

        match err {
            TicketingError::Jira(JiraError::NotFound { key }) => assert_eq!(key, "PROJ-404"),
            other => panic!("expected Jira NotFound, got {other:?}"),
        }
    }

    #[test]
    fn transition_ticket_propagates_transitions_fetch_error() {
        let jira = FakeJiraClient::new()
            .with_issue("PROJ-1", issue("PROJ-1"))
            .with_transitions_error("PROJ-1", 500, "fetch boom");

        let err = transition_ticket(&jira, "PROJ-1", "In Review").expect_err("should fail");

        match err {
            TicketingError::Jira(JiraError::Api { status, message }) => {
                assert_eq!(status, 500);
                assert_eq!(message, "fetch boom");
            }
            other => panic!("expected Jira Api error, got {other:?}"),
        }
    }

    #[test]
    fn transition_ticket_propagates_transition_api_error() {
        let jira = FakeJiraClient::new()
            .with_issue("PROJ-1", issue("PROJ-1"))
            .with_transitions(
                "PROJ-1",
                vec![transition("21", "Send to review", "In Review")],
            )
            .with_transition_error(500, "boom");

        let err = transition_ticket(&jira, "PROJ-1", "In Review").expect_err("should fail");

        match err {
            TicketingError::Jira(JiraError::Api { status, message }) => {
                assert_eq!(status, 500);
                assert_eq!(message, "boom");
            }
            other => panic!("expected Jira Api error, got {other:?}"),
        }
    }

    #[test]
    fn list_transitions_returns_current_status_and_transitions() {
        let jira = FakeJiraClient::new()
            .with_issue("PROJ-1", issue("PROJ-1"))
            .with_transitions(
                "PROJ-1",
                vec![transition("11", "Start Progress", "In Progress")],
            );

        let listing = list_transitions(&jira, "PROJ-1").expect("should succeed");

        assert_eq!(listing.current_status, "To Do");
        assert_eq!(listing.transitions.len(), 1);
        assert_eq!(listing.transitions[0].name, "Start Progress");
        assert_eq!(listing.transitions[0].to.name, "In Progress");
    }

    #[test]
    fn list_transitions_propagates_get_issue_error() {
        let jira = FakeJiraClient::new().with_issue_not_found("PROJ-404");

        let err = list_transitions(&jira, "PROJ-404").expect_err("should fail");

        match err {
            TicketingError::Jira(JiraError::NotFound { key }) => assert_eq!(key, "PROJ-404"),
            other => panic!("expected Jira NotFound, got {other:?}"),
        }
    }

    #[test]
    fn list_transitions_propagates_transitions_fetch_error() {
        let jira = FakeJiraClient::new()
            .with_issue("PROJ-1", issue("PROJ-1"))
            .with_transitions_error("PROJ-1", 500, "fetch boom");

        let err = list_transitions(&jira, "PROJ-1").expect_err("should fail");

        match err {
            TicketingError::Jira(JiraError::Api { status, message }) => {
                assert_eq!(status, 500);
                assert_eq!(message, "fetch boom");
            }
            other => panic!("expected Jira Api error, got {other:?}"),
        }
    }

    fn jira_user(account_id: &str, display_name: &str) -> JiraUser {
        JiraUser {
            account_id: account_id.to_string(),
            display_name: display_name.to_string(),
        }
    }

    #[test]
    fn assign_ticket_by_name_exact_match_assigns() {
        let jira = FakeJiraClient::new().with_assignable_users(
            "PROJ",
            vec![
                jira_user("acct-1", "Ada Lovelace"),
                jira_user("acct-2", "Jane Doe"),
            ],
        );
        let cfg = config();

        let outcome = assign_ticket(
            &jira,
            &cfg,
            "PROJ-372",
            &AssignTarget::Name("Jane Doe".to_string()),
        )
        .expect("should succeed");

        assert_eq!(
            outcome,
            AssignOutcome::AssignedToUser("Jane Doe".to_string())
        );
        assert_eq!(
            jira.assign_calls(),
            vec![("PROJ-372".to_string(), Some("acct-2".to_string()))]
        );
    }

    #[test]
    fn assign_ticket_by_name_is_case_insensitive() {
        let jira = FakeJiraClient::new()
            .with_assignable_users("PROJ", vec![jira_user("acct-1", "Jane Doe")]);
        let cfg = config();

        let outcome = assign_ticket(
            &jira,
            &cfg,
            "PROJ-372",
            &AssignTarget::Name("jane doe".to_string()),
        )
        .expect("should succeed");

        assert_eq!(
            outcome,
            AssignOutcome::AssignedToUser("Jane Doe".to_string())
        );
    }

    #[test]
    fn assign_ticket_by_name_falls_back_to_unambiguous_substring_match() {
        let jira = FakeJiraClient::new().with_assignable_users(
            "PROJ",
            vec![
                jira_user("acct-1", "Jane Doe"),
                jira_user("acct-2", "John Smith"),
            ],
        );
        let cfg = config();

        let outcome = assign_ticket(
            &jira,
            &cfg,
            "PROJ-372",
            &AssignTarget::Name("jane".to_string()),
        )
        .expect("should succeed");

        assert_eq!(
            outcome,
            AssignOutcome::AssignedToUser("Jane Doe".to_string())
        );
        assert_eq!(
            jira.assign_calls(),
            vec![("PROJ-372".to_string(), Some("acct-1".to_string()))]
        );
    }

    #[test]
    fn assign_ticket_by_name_ambiguous_substring_match_is_a_hard_error_listing_candidates() {
        let jira = FakeJiraClient::new().with_assignable_users(
            "PROJ",
            vec![
                jira_user("acct-1", "Jane Doe"),
                jira_user("acct-2", "Jane Smith"),
            ],
        );
        let cfg = config();

        let err = assign_ticket(
            &jira,
            &cfg,
            "PROJ-372",
            &AssignTarget::Name("jane".to_string()),
        )
        .expect_err("should fail");

        match err {
            TicketingError::NoMatchingAssignee {
                key,
                name,
                available,
            } => {
                assert_eq!(key, "PROJ-372");
                assert_eq!(name, "jane");
                assert!(available.contains("Jane Doe") && available.contains("Jane Smith"));
            }
            other => panic!("expected NoMatchingAssignee, got {other:?}"),
        }
        assert!(jira.assign_calls().is_empty());
    }

    #[test]
    fn assign_ticket_by_name_duplicate_exact_display_name_is_a_hard_error_listing_account_ids() {
        // Real Jira data: project AX has two distinct accountIds sharing the
        // exact displayName "Reports & Timesheets AI Agent". An exact match
        // must not silently pick whichever the API happened to list first.
        let jira = FakeJiraClient::new().with_assignable_users(
            "PROJ",
            vec![
                jira_user("acct-1", "Reports & Timesheets AI Agent"),
                jira_user("acct-2", "Reports & Timesheets AI Agent"),
            ],
        );
        let cfg = config();

        let err = assign_ticket(
            &jira,
            &cfg,
            "PROJ-372",
            &AssignTarget::Name("Reports & Timesheets AI Agent".to_string()),
        )
        .expect_err("should fail: two users share this exact displayName");

        match err {
            TicketingError::NoMatchingAssignee {
                key,
                name,
                available,
            } => {
                assert_eq!(key, "PROJ-372");
                assert_eq!(name, "Reports & Timesheets AI Agent");
                assert!(
                    available.contains("acct-1") && available.contains("acct-2"),
                    "available should list account IDs to disambiguate identically-named users: {available}"
                );
            }
            other => panic!("expected NoMatchingAssignee, got {other:?}"),
        }
        assert!(jira.assign_calls().is_empty());
    }

    #[test]
    fn assign_ticket_by_name_empty_name_is_a_hard_error() {
        // A single-user project would otherwise let the substring rule match
        // trivially on an empty needle ("".contains("") is always true),
        // silently assigning. This must be rejected explicitly instead.
        let jira = FakeJiraClient::new()
            .with_assignable_users("PROJ", vec![jira_user("acct-1", "Jane Doe")]);
        let cfg = config();

        let err = assign_ticket(&jira, &cfg, "PROJ-372", &AssignTarget::Name("".to_string()))
            .expect_err("empty name should fail");

        match err {
            TicketingError::EmptyAssigneeName { key } => assert_eq!(key, "PROJ-372"),
            other => panic!("expected EmptyAssigneeName, got {other:?}"),
        }
        assert!(jira.assign_calls().is_empty());
    }

    #[test]
    fn assign_ticket_by_name_whitespace_only_name_is_a_hard_error() {
        let jira = FakeJiraClient::new()
            .with_assignable_users("PROJ", vec![jira_user("acct-1", "Jane Doe")]);
        let cfg = config();

        let err = assign_ticket(
            &jira,
            &cfg,
            "PROJ-372",
            &AssignTarget::Name("   ".to_string()),
        )
        .expect_err("whitespace-only name should fail");

        assert!(matches!(err, TicketingError::EmptyAssigneeName { .. }));
        assert!(jira.assign_calls().is_empty());
    }

    #[test]
    fn assign_ticket_by_name_no_match_lists_all_assignable_users() {
        let jira = FakeJiraClient::new().with_assignable_users(
            "PROJ",
            vec![
                jira_user("acct-1", "Jane Doe"),
                jira_user("acct-2", "John Smith"),
            ],
        );
        let cfg = config();

        let err = assign_ticket(
            &jira,
            &cfg,
            "PROJ-372",
            &AssignTarget::Name("nobody".to_string()),
        )
        .expect_err("should fail");

        match err {
            TicketingError::NoMatchingAssignee { available, .. } => {
                assert!(available.contains("Jane Doe") && available.contains("John Smith"));
                assert!(available.contains("assignable users"));
            }
            other => panic!("expected NoMatchingAssignee, got {other:?}"),
        }
    }

    #[test]
    fn assign_ticket_by_name_no_assignable_users_at_all_says_so() {
        let jira = FakeJiraClient::new().with_assignable_users("PROJ", vec![]);
        let cfg = config();

        let err = assign_ticket(
            &jira,
            &cfg,
            "PROJ-372",
            &AssignTarget::Name("nobody".to_string()),
        )
        .expect_err("should fail");

        match err {
            TicketingError::NoMatchingAssignee { available, .. } => {
                assert_eq!(available, "no assignable users found in the project");
            }
            other => panic!("expected NoMatchingAssignee, got {other:?}"),
        }
    }

    #[test]
    fn assign_ticket_by_name_uses_projects_own_prefix_not_default_project() {
        let jira = FakeJiraClient::new()
            .with_assignable_users("OTHER", vec![jira_user("acct-1", "Jane Doe")]);
        let cfg = config(); // default_project_key is "PROJ"

        let outcome = assign_ticket(
            &jira,
            &cfg,
            "OTHER-9",
            &AssignTarget::Name("Jane Doe".to_string()),
        )
        .expect("should succeed even though OTHER != config's default project");

        assert_eq!(
            outcome,
            AssignOutcome::AssignedToUser("Jane Doe".to_string())
        );
    }

    #[test]
    fn assign_ticket_by_name_propagates_assignable_users_error() {
        let jira = FakeJiraClient::new().with_assignable_users_error("PROJ", 500, "boom");
        let cfg = config();

        let err = assign_ticket(
            &jira,
            &cfg,
            "PROJ-372",
            &AssignTarget::Name("Jane".to_string()),
        )
        .expect_err("should fail");

        match err {
            TicketingError::Jira(JiraError::Api { status, message }) => {
                assert_eq!(status, 500);
                assert_eq!(message, "boom");
            }
            other => panic!("expected Jira Api error, got {other:?}"),
        }
    }

    #[test]
    fn assign_ticket_by_name_propagates_assign_api_error() {
        let jira = FakeJiraClient::new()
            .with_assignable_users("PROJ", vec![jira_user("acct-1", "Jane Doe")])
            .with_assign_error(500, "boom");
        let cfg = config();

        let err = assign_ticket(
            &jira,
            &cfg,
            "PROJ-372",
            &AssignTarget::Name("Jane Doe".to_string()),
        )
        .expect_err("should fail");

        match err {
            TicketingError::Jira(JiraError::Api { status, message }) => {
                assert_eq!(status, 500);
                assert_eq!(message, "boom");
            }
            other => panic!("expected Jira Api error, got {other:?}"),
        }
    }

    #[test]
    fn assign_ticket_me_uses_cached_account_id_without_calling_myself() {
        let jira = FakeJiraClient::new();
        let cfg = config(); // default_assignee_account_id is Some("acct-1")

        let outcome =
            assign_ticket(&jira, &cfg, "PROJ-372", &AssignTarget::Me).expect("should succeed");

        assert_eq!(outcome, AssignOutcome::AssignedToMe("acct-1".to_string()));
        assert_eq!(
            jira.assign_calls(),
            vec![("PROJ-372".to_string(), Some("acct-1".to_string()))]
        );
    }

    #[test]
    fn assign_ticket_me_falls_back_to_myself_when_no_cached_account_id() {
        let jira = FakeJiraClient::new().with_myself(Myself {
            account_id: "acct-me".to_string(),
            display_name: "Ada Lovelace".to_string(),
            email_address: None,
        });
        let cfg = Config {
            default_assignee_account_id: None,
            ..config()
        };

        let outcome =
            assign_ticket(&jira, &cfg, "PROJ-372", &AssignTarget::Me).expect("should succeed");

        assert_eq!(
            outcome,
            AssignOutcome::AssignedToMe("Ada Lovelace".to_string())
        );
        assert_eq!(
            jira.assign_calls(),
            vec![("PROJ-372".to_string(), Some("acct-me".to_string()))]
        );
    }

    #[test]
    fn assign_ticket_me_propagates_myself_error() {
        let jira = FakeJiraClient::new().with_myself_unauthorized();
        let cfg = Config {
            default_assignee_account_id: None,
            ..config()
        };

        let err =
            assign_ticket(&jira, &cfg, "PROJ-372", &AssignTarget::Me).expect_err("should fail");

        assert!(matches!(err, TicketingError::Jira(JiraError::Unauthorized)));
    }

    #[test]
    fn assign_ticket_unassign_clears_assignee() {
        let jira = FakeJiraClient::new();
        let cfg = config();

        let outcome = assign_ticket(&jira, &cfg, "PROJ-372", &AssignTarget::Unassign)
            .expect("should succeed");

        assert_eq!(outcome, AssignOutcome::Unassigned);
        assert_eq!(jira.assign_calls(), vec![("PROJ-372".to_string(), None)]);
    }

    #[test]
    fn assign_ticket_unassign_propagates_assign_error() {
        let jira = FakeJiraClient::new().with_assign_error(500, "boom");
        let cfg = config();

        let err = assign_ticket(&jira, &cfg, "PROJ-372", &AssignTarget::Unassign)
            .expect_err("should fail");

        match err {
            TicketingError::Jira(JiraError::Api { status, message }) => {
                assert_eq!(status, 500);
                assert_eq!(message, "boom");
            }
            other => panic!("expected Jira Api error, got {other:?}"),
        }
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
        let jira = FakeJiraClient::new().with_issue("PROJ-372", issue("PROJ-372"));
        let pull_request = pr("Fix the thing");

        let key = resolve_existing_key(&jira, &pull_request).expect("should succeed");

        assert_eq!(key, Some("PROJ-372".to_string()));
    }

    #[test]
    fn resolve_existing_key_branch_key_not_found_is_none() {
        let jira = FakeJiraClient::new().with_issue_not_found("PROJ-372");
        let pull_request = pr("Fix the thing");

        let key = resolve_existing_key(&jira, &pull_request).expect("should succeed");

        assert_eq!(key, None);
    }

    #[test]
    fn resolve_existing_key_branch_key_other_error_propagates() {
        let jira = FakeJiraClient::new().with_issue_error("PROJ-372", 500, "boom");
        let pull_request = pr("Fix the thing");

        let err = resolve_existing_key(&jira, &pull_request).expect_err("should fail");

        match err {
            TicketingError::Jira(JiraError::Api { status, .. }) => assert_eq!(status, 500),
            other => panic!("expected Jira Api error, got {other:?}"),
        }
    }

    #[test]
    fn rank_ticket_ranks_before_anchor() {
        let jira = FakeJiraClient::new().with_issue("PROJ-1", issue("PROJ-1"));

        rank_ticket(&jira, "PROJ-1", RankAnchor::Before("PROJ-2".to_string()))
            .expect("should succeed");

        assert_eq!(
            jira.rank_calls(),
            vec![(
                vec!["PROJ-1".to_string()],
                RankAnchor::Before("PROJ-2".to_string())
            )]
        );
    }

    #[test]
    fn rank_ticket_ranks_after_anchor() {
        let jira = FakeJiraClient::new().with_issue("PROJ-1", issue("PROJ-1"));

        rank_ticket(&jira, "PROJ-1", RankAnchor::After("PROJ-2".to_string()))
            .expect("should succeed");

        assert_eq!(
            jira.rank_calls(),
            vec![(
                vec!["PROJ-1".to_string()],
                RankAnchor::After("PROJ-2".to_string())
            )]
        );
    }

    #[test]
    fn rank_ticket_missing_primary_key_errors_with_key_before_calling_rank() {
        let jira = FakeJiraClient::new().with_issue_not_found("PROJ-404");

        let err = rank_ticket(&jira, "PROJ-404", RankAnchor::Before("PROJ-2".to_string()))
            .expect_err("should fail");

        match err {
            TicketingError::Jira(JiraError::NotFound { key }) => assert_eq!(key, "PROJ-404"),
            other => panic!("expected Jira NotFound, got {other:?}"),
        }
        assert!(
            jira.rank_calls().is_empty(),
            "should not call rank when the primary key doesn't exist"
        );
    }

    #[test]
    fn rank_ticket_propagates_rank_api_error() {
        let jira = FakeJiraClient::new()
            .with_issue("PROJ-1", issue("PROJ-1"))
            .with_rank_error(500, "boom");

        let err = rank_ticket(&jira, "PROJ-1", RankAnchor::Before("PROJ-2".to_string()))
            .expect_err("should fail");

        match err {
            TicketingError::Jira(JiraError::Api { status, message }) => {
                assert_eq!(status, 500);
                assert_eq!(message, "boom");
            }
            other => panic!("expected Jira Api error, got {other:?}"),
        }
    }

    #[test]
    fn link_ticket_creates_link() {
        let jira = FakeJiraClient::new().with_issue("PROJ-1", issue("PROJ-1"));
        let req = CreateLinkRequest {
            blocker_key: "PROJ-1".to_string(),
            blocked_key: "PROJ-2".to_string(),
        };

        link_ticket(&jira, "PROJ-1", &req).expect("should succeed");

        assert_eq!(jira.create_link_calls(), vec![req]);
    }

    #[test]
    fn link_ticket_missing_primary_key_errors_before_calling_create_link() {
        let jira = FakeJiraClient::new().with_issue_not_found("PROJ-404");
        let req = CreateLinkRequest {
            blocker_key: "PROJ-404".to_string(),
            blocked_key: "PROJ-2".to_string(),
        };

        let err = link_ticket(&jira, "PROJ-404", &req).expect_err("should fail");

        match err {
            TicketingError::Jira(JiraError::NotFound { key }) => assert_eq!(key, "PROJ-404"),
            other => panic!("expected Jira NotFound, got {other:?}"),
        }
        assert!(
            jira.create_link_calls().is_empty(),
            "should not call create_link when the primary key doesn't exist"
        );
    }

    #[test]
    fn link_ticket_propagates_create_link_api_error() {
        let jira = FakeJiraClient::new()
            .with_issue("PROJ-1", issue("PROJ-1"))
            .with_create_link_error(500, "boom");
        let req = CreateLinkRequest {
            blocker_key: "PROJ-1".to_string(),
            blocked_key: "PROJ-2".to_string(),
        };

        let err = link_ticket(&jira, "PROJ-1", &req).expect_err("should fail");

        match err {
            TicketingError::Jira(JiraError::Api { status, message }) => {
                assert_eq!(status, 500);
                assert_eq!(message, "boom");
            }
            other => panic!("expected Jira Api error, got {other:?}"),
        }
    }

    #[test]
    fn list_links_returns_issue_links() {
        let mut with_links = issue("PROJ-1");
        with_links.fields.issue_links = vec![IssueLink {
            id: "10001".to_string(),
            link_type: crate::jira::types::IssueLinkType {
                name: "Blocks".to_string(),
                inward: "is blocked by".to_string(),
                outward: "blocks".to_string(),
            },
            inward_issue: Some(crate::jira::types::LinkedIssue {
                key: "PROJ-2".to_string(),
                fields: crate::jira::types::LinkedIssueFields {
                    summary: "Blocker ticket".to_string(),
                    status: Status {
                        name: "In Progress".to_string(),
                        status_category: StatusCategory {
                            key: "indeterminate".to_string(),
                        },
                    },
                },
            }),
            outward_issue: None,
        }];
        let jira = FakeJiraClient::new().with_issue("PROJ-1", with_links);

        let listing = list_links(&jira, "PROJ-1").expect("should succeed");

        assert_eq!(listing.links.len(), 1);
        assert_eq!(
            listing.links[0].inward_issue.as_ref().unwrap().key,
            "PROJ-2"
        );
    }

    #[test]
    fn list_links_propagates_get_issue_error() {
        let jira = FakeJiraClient::new().with_issue_not_found("PROJ-404");

        let err = list_links(&jira, "PROJ-404").expect_err("should fail");

        match err {
            TicketingError::Jira(JiraError::NotFound { key }) => assert_eq!(key, "PROJ-404"),
            other => panic!("expected Jira NotFound, got {other:?}"),
        }
    }

    #[test]
    fn unlink_ticket_removes_inward_blocks_link() {
        let mut i = issue("PROJ-1");
        i.fields.issue_links = vec![IssueLink {
            id: "10001".to_string(),
            link_type: blocks_link_type(),
            inward_issue: Some(linked_issue("PROJ-2", "In Progress", "indeterminate")),
            outward_issue: None,
        }];
        let jira = FakeJiraClient::new().with_issue("PROJ-1", i);

        let outcome = unlink_ticket(&jira, "PROJ-1", "PROJ-2").expect("should succeed");

        assert_eq!(outcome.removed, vec!["is blocked by".to_string()]);
        assert_eq!(jira.delete_link_calls(), vec!["10001".to_string()]);
    }

    #[test]
    fn unlink_ticket_removes_outward_blocks_link() {
        let mut i = issue("PROJ-1");
        i.fields.issue_links = vec![IssueLink {
            id: "10002".to_string(),
            link_type: blocks_link_type(),
            inward_issue: None,
            outward_issue: Some(linked_issue("PROJ-2", "To Do", "new")),
        }];
        let jira = FakeJiraClient::new().with_issue("PROJ-1", i);

        let outcome = unlink_ticket(&jira, "PROJ-1", "PROJ-2").expect("should succeed");

        assert_eq!(outcome.removed, vec!["blocks".to_string()]);
        assert_eq!(jira.delete_link_calls(), vec!["10002".to_string()]);
    }

    #[test]
    fn unlink_ticket_removes_both_directions_when_both_exist() {
        let mut i = issue("PROJ-1");
        i.fields.issue_links = vec![
            IssueLink {
                id: "10001".to_string(),
                link_type: blocks_link_type(),
                inward_issue: Some(linked_issue("PROJ-2", "In Progress", "indeterminate")),
                outward_issue: None,
            },
            IssueLink {
                id: "10002".to_string(),
                link_type: blocks_link_type(),
                inward_issue: None,
                outward_issue: Some(linked_issue("PROJ-2", "In Progress", "indeterminate")),
            },
        ];
        let jira = FakeJiraClient::new().with_issue("PROJ-1", i);

        let outcome = unlink_ticket(&jira, "PROJ-1", "PROJ-2").expect("should succeed");

        assert_eq!(
            outcome.removed,
            vec!["is blocked by".to_string(), "blocks".to_string()]
        );
        assert_eq!(
            jira.delete_link_calls(),
            vec!["10001".to_string(), "10002".to_string()]
        );
    }

    #[test]
    fn unlink_ticket_no_links_at_all_is_a_hard_error_with_empty_others() {
        let jira = FakeJiraClient::new().with_issue("PROJ-1", issue("PROJ-1"));

        let err = unlink_ticket(&jira, "PROJ-1", "PROJ-2").expect_err("should fail");

        match err {
            TicketingError::NoBlocksLinkBetween { key, other, others } => {
                assert_eq!(key, "PROJ-1");
                assert_eq!(other, "PROJ-2");
                assert_eq!(others, "");
            }
            other => panic!("expected NoBlocksLinkBetween, got {other:?}"),
        }
        assert!(jira.delete_link_calls().is_empty());
    }

    #[test]
    fn unlink_ticket_only_relates_link_between_pair_names_it_in_the_error() {
        let mut i = issue("PROJ-1");
        i.fields.issue_links = vec![IssueLink {
            id: "10003".to_string(),
            link_type: relates_link_type(),
            inward_issue: Some(linked_issue("PROJ-2", "To Do", "new")),
            outward_issue: None,
        }];
        let jira = FakeJiraClient::new().with_issue("PROJ-1", i);

        let err = unlink_ticket(&jira, "PROJ-1", "PROJ-2").expect_err("should fail");

        match err {
            TicketingError::NoBlocksLinkBetween { others, .. } => {
                assert_eq!(others, "relates to PROJ-2");
            }
            other => panic!("expected NoBlocksLinkBetween, got {other:?}"),
        }
        assert!(jira.delete_link_calls().is_empty());
    }

    #[test]
    fn unlink_ticket_error_message_includes_other_links_summary() {
        let mut i = issue("PROJ-1");
        i.fields.issue_links = vec![IssueLink {
            id: "10003".to_string(),
            link_type: relates_link_type(),
            inward_issue: Some(linked_issue("PROJ-2", "To Do", "new")),
            outward_issue: None,
        }];
        let jira = FakeJiraClient::new().with_issue("PROJ-1", i);

        let err = unlink_ticket(&jira, "PROJ-1", "PROJ-2").expect_err("should fail");

        let message = err.to_string();
        assert!(message.contains("no Blocks link between PROJ-1 and PROJ-2"));
        assert!(message.contains("other links exist: relates to PROJ-2"));
    }

    #[test]
    fn unlink_ticket_blocks_link_to_a_different_issue_is_untouched() {
        let mut i = issue("PROJ-1");
        i.fields.issue_links = vec![IssueLink {
            id: "10004".to_string(),
            link_type: blocks_link_type(),
            inward_issue: Some(linked_issue("PROJ-9", "In Progress", "indeterminate")),
            outward_issue: None,
        }];
        let jira = FakeJiraClient::new().with_issue("PROJ-1", i);

        let err = unlink_ticket(&jira, "PROJ-1", "PROJ-2").expect_err("should fail");

        assert!(matches!(err, TicketingError::NoBlocksLinkBetween { .. }));
        assert!(jira.delete_link_calls().is_empty());
    }

    #[test]
    fn unlink_ticket_missing_primary_key_passes_through_get_issue_error() {
        let jira = FakeJiraClient::new().with_issue_not_found("PROJ-404");

        let err = unlink_ticket(&jira, "PROJ-404", "PROJ-2").expect_err("should fail");

        match err {
            TicketingError::Jira(JiraError::NotFound { key }) => assert_eq!(key, "PROJ-404"),
            other => panic!("expected Jira NotFound, got {other:?}"),
        }
        assert!(jira.delete_link_calls().is_empty());
    }

    #[test]
    fn unlink_ticket_propagates_delete_link_api_error() {
        let mut i = issue("PROJ-1");
        i.fields.issue_links = vec![IssueLink {
            id: "10001".to_string(),
            link_type: blocks_link_type(),
            inward_issue: Some(linked_issue("PROJ-2", "In Progress", "indeterminate")),
            outward_issue: None,
        }];
        let jira = FakeJiraClient::new()
            .with_issue("PROJ-1", i)
            .with_delete_link_error(500, "boom");

        let err = unlink_ticket(&jira, "PROJ-1", "PROJ-2").expect_err("should fail");

        match err {
            TicketingError::Jira(JiraError::Api { status, message }) => {
                assert_eq!(status, 500);
                assert_eq!(message, "boom");
            }
            other => panic!("expected Jira Api error, got {other:?}"),
        }
    }

    fn linked_issue(key: &str, status_name: &str, status_category_key: &str) -> LinkedIssue {
        LinkedIssue {
            key: key.to_string(),
            fields: crate::jira::types::LinkedIssueFields {
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

    fn blocks_link_type() -> crate::jira::types::IssueLinkType {
        crate::jira::types::IssueLinkType {
            name: "Blocks".to_string(),
            inward: "is blocked by".to_string(),
            outward: "blocks".to_string(),
        }
    }

    fn relates_link_type() -> crate::jira::types::IssueLinkType {
        crate::jira::types::IssueLinkType {
            name: "Relates".to_string(),
            inward: "relates to".to_string(),
            outward: "relates to".to_string(),
        }
    }

    #[test]
    fn open_blockers_with_no_links_is_empty() {
        let i = issue("PROJ-1");
        assert!(open_blockers(&i).is_empty());
    }

    #[test]
    fn open_blockers_ignores_outward_only_blocks_link() {
        let mut i = issue("PROJ-1");
        i.fields.issue_links = vec![IssueLink {
            id: "10001".to_string(),
            link_type: blocks_link_type(),
            inward_issue: None,
            outward_issue: Some(linked_issue("PROJ-2", "To Do", "new")),
        }];
        assert!(
            open_blockers(&i).is_empty(),
            "an outward Blocks entry means PROJ-1 blocks PROJ-2, not the reverse"
        );
    }

    #[test]
    fn open_blockers_includes_inward_blocks_link_when_not_done() {
        let mut i = issue("PROJ-1");
        i.fields.issue_links = vec![IssueLink {
            id: "10001".to_string(),
            link_type: blocks_link_type(),
            inward_issue: Some(linked_issue("PROJ-2", "In Progress", "indeterminate")),
            outward_issue: None,
        }];
        let blockers = open_blockers(&i);
        assert_eq!(blockers.len(), 1);
        assert_eq!(blockers[0].key, "PROJ-2");
    }

    #[test]
    fn open_blockers_excludes_inward_blocks_link_when_done() {
        let mut i = issue("PROJ-1");
        i.fields.issue_links = vec![IssueLink {
            id: "10001".to_string(),
            link_type: blocks_link_type(),
            inward_issue: Some(linked_issue("PROJ-2", "Done", "done")),
            outward_issue: None,
        }];
        assert!(
            open_blockers(&i).is_empty(),
            "a Done blocker should not count as an open blocker"
        );
    }

    #[test]
    fn open_blockers_ignores_inward_link_of_a_different_type() {
        let mut i = issue("PROJ-1");
        i.fields.issue_links = vec![IssueLink {
            id: "10001".to_string(),
            link_type: relates_link_type(),
            inward_issue: Some(linked_issue("PROJ-2", "To Do", "new")),
            outward_issue: None,
        }];
        assert!(open_blockers(&i).is_empty());
    }

    #[test]
    fn open_blockers_mixed_links_returns_only_open_blocks_blockers() {
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
            IssueLink {
                id: "10001".to_string(),
                link_type: blocks_link_type(),
                inward_issue: None,
                outward_issue: Some(linked_issue("PROJ-4", "To Do", "new")),
            },
            IssueLink {
                id: "10001".to_string(),
                link_type: relates_link_type(),
                inward_issue: Some(linked_issue("PROJ-5", "To Do", "new")),
                outward_issue: None,
            },
        ];
        let blockers = open_blockers(&i);
        assert_eq!(blockers.len(), 1);
        assert_eq!(blockers[0].key, "PROJ-2");
    }

    fn search_result(issues: Vec<Issue>) -> crate::jira::types::SearchResult {
        crate::jira::types::SearchResult {
            issues,
            next_page_token: None,
        }
    }

    #[test]
    fn ready_tickets_keeps_candidates_with_no_open_blockers_and_preserves_order() {
        let blocked = {
            let mut i = issue("PROJ-2");
            i.fields.issue_links = vec![IssueLink {
                id: "10001".to_string(),
                link_type: blocks_link_type(),
                inward_issue: Some(linked_issue("PROJ-9", "In Progress", "indeterminate")),
                outward_issue: None,
            }];
            i
        };
        let jira = FakeJiraClient::new().with_search_result(search_result(vec![
            issue("PROJ-1"),
            blocked,
            issue("PROJ-3"),
        ]));

        let listing = ready_tickets(&jira).expect("should succeed");

        assert_eq!(
            listing
                .ready
                .iter()
                .map(|i| i.key.clone())
                .collect::<Vec<_>>(),
            vec!["PROJ-1".to_string(), "PROJ-3".to_string()]
        );
        assert_eq!(listing.hidden_blocked_count(), 1);
    }

    #[test]
    fn ready_tickets_with_no_candidates_is_empty_with_zero_hidden() {
        let jira = FakeJiraClient::new().with_search_result(search_result(vec![]));

        let listing = ready_tickets(&jira).expect("should succeed");

        assert!(listing.ready.is_empty());
        assert_eq!(listing.hidden_blocked_count(), 0);
    }

    #[test]
    fn ready_tickets_propagates_search_error() {
        let jira = FakeJiraClient::new().with_search_error(500, "boom");

        let err = ready_tickets(&jira).expect_err("should fail");

        match err {
            TicketingError::Jira(JiraError::Api { status, message }) => {
                assert_eq!(status, 500);
                assert_eq!(message, "boom");
            }
            other => panic!("expected Jira Api error, got {other:?}"),
        }
    }

    #[test]
    fn check_ready_with_no_open_blockers_reports_ready() {
        let jira = FakeJiraClient::new().with_issue("PROJ-1", issue("PROJ-1"));

        let check = check_ready(&jira, "PROJ-1").expect("should succeed");

        assert_eq!(check.status_name, "To Do");
        assert!(check.open_blockers.is_empty());
    }

    #[test]
    fn check_ready_with_open_blocker_reports_it_and_excludes_done_blocker() {
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

        let check = check_ready(&jira, "PROJ-1").expect("should succeed");

        assert_eq!(check.open_blockers.len(), 1);
        assert_eq!(check.open_blockers[0].key, "PROJ-2");
    }

    #[test]
    fn check_ready_propagates_get_issue_error() {
        let jira = FakeJiraClient::new().with_issue_not_found("PROJ-404");

        let err = check_ready(&jira, "PROJ-404").expect_err("should fail");

        match err {
            TicketingError::Jira(JiraError::NotFound { key }) => assert_eq!(key, "PROJ-404"),
            other => panic!("expected Jira NotFound, got {other:?}"),
        }
    }

    #[test]
    fn search_tickets_returns_search_results_in_order() {
        let jira = FakeJiraClient::new()
            .with_search_result(search_result(vec![issue("PROJ-1"), issue("PROJ-2")]));
        let cfg = config();

        let issues = search_tickets(&jira, &cfg, "login bug").expect("should succeed");

        assert_eq!(
            issues.iter().map(|i| i.key.clone()).collect::<Vec<_>>(),
            vec!["PROJ-1".to_string(), "PROJ-2".to_string()]
        );
    }

    #[test]
    fn search_tickets_with_no_matches_is_empty() {
        let jira = FakeJiraClient::new().with_search_result(search_result(vec![]));
        let cfg = config();

        let issues = search_tickets(&jira, &cfg, "login bug").expect("should succeed");

        assert!(issues.is_empty());
    }

    #[test]
    fn search_tickets_rejects_empty_text() {
        let jira = FakeJiraClient::new();
        let cfg = config();

        let err = search_tickets(&jira, &cfg, "").expect_err("empty text should fail");

        assert!(matches!(err, TicketingError::EmptySearchText));
    }

    #[test]
    fn search_tickets_rejects_whitespace_only_text() {
        let jira = FakeJiraClient::new();
        let cfg = config();

        let err = search_tickets(&jira, &cfg, "   ").expect_err("whitespace-only text should fail");

        assert!(matches!(err, TicketingError::EmptySearchText));
    }

    #[test]
    fn search_tickets_propagates_search_error() {
        let jira = FakeJiraClient::new().with_search_error(500, "boom");
        let cfg = config();

        let err = search_tickets(&jira, &cfg, "login bug").expect_err("should fail");

        match err {
            TicketingError::Jira(JiraError::Api { status, message }) => {
                assert_eq!(status, 500);
                assert_eq!(message, "boom");
            }
            other => panic!("expected Jira Api error, got {other:?}"),
        }
    }

    #[test]
    fn comment_ticket_explicit_key_posts_adf_comment_and_no_pr() {
        let jira = FakeJiraClient::new().with_issue("PROJ-1", issue("PROJ-1"));
        let gh = FakeGhCli::new();
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };

        let outcome =
            comment_ticket(&ctx, Some("PROJ-1"), "**bold** note", false).expect("should succeed");

        assert_eq!(outcome.issue_key, Some("PROJ-1".to_string()));
        assert_eq!(outcome.pr_number, None);

        let calls = jira.add_comment_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "PROJ-1");
        assert!(
            calls[0].1.to_string().contains("\"strong\""),
            "markdown body should be converted to ADF marks: {}",
            calls[0].1
        );
        assert!(gh.pr_comment_calls().is_empty());
    }

    #[test]
    fn comment_ticket_explicit_key_not_found_errors_with_key() {
        let jira = FakeJiraClient::new().with_issue_not_found("PROJ-404");
        let gh = FakeGhCli::new();
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };

        let err = comment_ticket(&ctx, Some("PROJ-404"), "note", false).expect_err("should fail");

        match err {
            TicketingError::Jira(JiraError::NotFound { key }) => assert_eq!(key, "PROJ-404"),
            other => panic!("expected Jira NotFound, got {other:?}"),
        }
        assert!(jira.add_comment_calls().is_empty());
    }

    #[test]
    fn comment_ticket_explicit_key_also_pr_comments_on_current_branch_pr_with_raw_markdown() {
        let jira = FakeJiraClient::new().with_issue("PROJ-1", issue("PROJ-1"));
        let gh = FakeGhCli::new().with_pr_view(Ok(Some(pr("Fix the thing"))));
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };

        let outcome =
            comment_ticket(&ctx, Some("PROJ-1"), "**bold** note", true).expect("should succeed");

        assert_eq!(outcome.pr_number, Some(42));
        assert_eq!(
            gh.pr_comment_calls(),
            vec![(42, "**bold** note".to_string())]
        );
    }

    #[test]
    fn comment_ticket_explicit_key_also_pr_no_pr_for_branch_is_a_hard_error() {
        let jira = FakeJiraClient::new().with_issue("PROJ-1", issue("PROJ-1"));
        let gh = FakeGhCli::new()
            .with_pr_view(Ok(None))
            .with_current_branch(Ok("proj-1-fix".to_string()));
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };

        let err = comment_ticket(&ctx, Some("PROJ-1"), "note", true).expect_err("should fail");

        match err {
            TicketingError::NoPrForBranch { branch } => assert_eq!(branch, "proj-1-fix"),
            other => panic!("expected NoPrForBranch, got {other:?}"),
        }
        // The Jira comment was already posted before the PR lookup failed;
        // this documents that ordering rather than asserting it never
        // happens.
        assert_eq!(jira.add_comment_calls().len(), 1);
    }

    #[test]
    fn comment_ticket_infers_key_from_current_branch_pr() {
        let jira = FakeJiraClient::new().with_issue("PROJ-372", issue("PROJ-372"));
        let mut pull_request = pr("[PROJ-372] Fix the thing");
        pull_request.head_ref_name = "some-other-branch".to_string();
        let gh = FakeGhCli::new().with_pr_view(Ok(Some(pull_request)));
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };

        let outcome = comment_ticket(&ctx, None, "note", false).expect("should succeed");

        assert_eq!(outcome.issue_key, Some("PROJ-372".to_string()));
        assert_eq!(jira.add_comment_calls()[0].0, "PROJ-372");
    }

    #[test]
    fn comment_ticket_infers_key_and_also_comments_on_pr_without_a_second_pr_view_lookup() {
        let jira = FakeJiraClient::new().with_issue("PROJ-372", issue("PROJ-372"));
        let mut pull_request = pr("[PROJ-372] Fix the thing");
        pull_request.head_ref_name = "some-other-branch".to_string();
        let gh = FakeGhCli::new().with_pr_view(Ok(Some(pull_request)));
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };

        let outcome = comment_ticket(&ctx, None, "note", true).expect("should succeed");

        assert_eq!(outcome.pr_number, Some(42));
        assert_eq!(gh.pr_comment_calls(), vec![(42, "note".to_string())]);
    }

    #[test]
    fn comment_ticket_no_key_no_pr_for_branch_is_no_ticket_or_pr_for_branch() {
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

        let err = comment_ticket(&ctx, None, "note", false).expect_err("should fail");

        match err {
            TicketingError::NoTicketOrPrForBranch { branch } => {
                assert_eq!(branch, "proj-372-fix")
            }
            other => panic!("expected NoTicketOrPrForBranch, got {other:?}"),
        }
        assert!(jira.add_comment_calls().is_empty());
    }

    #[test]
    fn comment_ticket_no_key_pr_with_no_resolvable_key_is_no_ticket_or_pr_for_branch() {
        let jira = FakeJiraClient::new();
        // A PR with no key in title/body and a branch name that doesn't look
        // like a ticket key: resolve_existing_key finds nothing.
        let mut pull_request = pr("Fix the thing");
        pull_request.head_ref_name = "some-branch".to_string();
        let gh = FakeGhCli::new()
            .with_pr_view(Ok(Some(pull_request)))
            .with_current_branch(Ok("some-branch".to_string()));
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };

        let err = comment_ticket(&ctx, None, "note", false).expect_err("should fail");

        match err {
            TicketingError::NoTicketOrPrForBranch { branch } => {
                assert_eq!(branch, "some-branch")
            }
            other => panic!("expected NoTicketOrPrForBranch, got {other:?}"),
        }
    }
}
