//! Orchestration of ticket <-> pull request association, and of ticket
//! creation independent of any pull request.
//!
//! This module ties together [`crate::jira`] and [`crate::github`]: given a
//! Jira issue key and the pull request open for the current branch, it makes
//! the PR title carry the key and posts a Jira remote link pointing at the
//! PR. It does not itself talk to the network; all I/O goes through the
//! [`JiraClient`] and [`GhCli`] trait objects on [`TicketingContext`].
//!
//! Three functions move a ticket to a configured workflow status after
//! creating or linking it, since Jira's create-issue API can't set status
//! directly: [`auto_create_and_associate`] (a fresh ticket auto-created
//! because a PR is already open), [`associate_existing_ticket_for_pr_create`]
//! (a pre-existing ticket that `tm pr create` links to a newly opened PR),
//! and [`create_ticket`] (a fresh ticket made by `tm ticket create`, with no
//! PR involved at all — see [`CreateTicketContext`], which deliberately has
//! no [`GhCli`] dependency). All three share the same matching logic via
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
//! [`JiraClient::rank`]) relative to another issue. Like transition and
//! assign, it's explicit: any Jira API failure is a hard error. It verifies
//! `KEY` exists first so a typo there gets a friendly [`JiraError::NotFound`];
//! a typo'd anchor key surfaces from the rank call itself as
//! [`JiraError::RankNotFound`].

use thiserror::Error;

use crate::config::Config;
use crate::github::gh_cli::{GhCli, GhError, PrEditRequest};
use crate::github::pr::{KeySource, PrInfo, find_issue_key_with_source, with_issue_key_prefix};
use crate::jira::adf::text_to_adf;
use crate::jira::client::{JiraClient, JiraError, RankAnchor};
use crate::jira::types::{CreateIssueRequest, JiraUser, RemoteLinkRequest};

/// Dependencies shared by the ticketing orchestration functions that deal
/// with a pull request.
pub struct TicketingContext<'a> {
    /// Jira client used to verify issues and post remote links.
    pub jira: &'a dyn JiraClient,
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
    /// Jira client used to create the issue and apply its status transition.
    pub jira: &'a dyn JiraClient,
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
/// Reuses the [`JiraClient::get_issue`] call `associate_ticket` already made
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
    /// Outcome of attempting to move the ticket to
    /// [`Config::status_on_create`], if configured. `None` when it isn't.
    pub status_transition: Option<StatusTransition>,
}

/// `tm ticket create`: create a new issue in the configured default project,
/// assigned to the configured default assignee, with no pull request
/// involved.
///
/// `body`, if given, is parsed as GitHub-flavored Markdown into the issue's
/// ADF description; when absent, the issue is created with an empty
/// description. If [`Config::status_on_create`] is configured, the new
/// ticket is moved to it via [`apply_status_transition`] — the same
/// case-insensitive matching used by the `tm pr create` paths.
pub fn create_ticket(
    ctx: &CreateTicketContext,
    title: &str,
    body: Option<&str>,
) -> Result<CreateTicketOutcome, TicketingError> {
    let description = text_to_adf(body.unwrap_or_default());
    let req = CreateIssueRequest {
        project_key: ctx.config.default_project_key.clone(),
        summary: title.to_string(),
        description,
        issue_type_name: "Task".to_string(),
        assignee_account_id: ctx.config.default_assignee_account_id.clone(),
    };
    let issue = ctx.jira.create_issue(&req)?;
    let status_transition = ctx
        .config
        .status_on_create
        .as_ref()
        .map(|target| apply_status_transition(ctx.jira, &issue.key, target));

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
fn apply_status_transition(jira: &dyn JiraClient, key: &str, target: &str) -> StatusTransition {
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
    jira: &dyn JiraClient,
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
    jira: &dyn JiraClient,
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
    jira: &dyn JiraClient,
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
/// Verifies `key` exists first (via [`JiraClient::get_issue`]) so a typo'd
/// primary key gives the same friendly [`JiraError::NotFound`] every other
/// `tm ticket` subcommand does, rather than surfacing as a raw
/// [`JiraError::RankNotFound`] from the agile API. A typo'd anchor key (in
/// `anchor`) is not checked ahead of time; it surfaces from the `rank` call
/// itself as [`JiraError::RankNotFound`], since Jira's rank endpoint reports
/// that case directly and a second lookup would be redundant.
pub fn rank_ticket(
    jira: &dyn JiraClient,
    key: &str,
    anchor: RankAnchor,
) -> Result<(), TicketingError> {
    jira.get_issue(key)?;
    jira.rank(&[key.to_string()], anchor)?;
    Ok(())
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
            jira_base_url: "https://example.atlassian.net".to_string(),
            jira_email: "ada@example.com".to_string(),
            default_project_key: "PROJ".to_string(),
            default_assignee_account_id: Some("acct-1".to_string()),
            status_on_pr: None,
            status_on_create: None,
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

        let outcome =
            create_ticket(&ctx, "Add the widget", Some("Some **body**")).expect("should succeed");

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

        create_ticket(&ctx, "Add the widget", None).expect("should succeed");

        let calls = jira.create_issue_calls();
        assert_eq!(
            calls[0].description,
            serde_json::json!({ "type": "doc", "version": 1, "content": [] })
        );
    }

    #[test]
    fn create_ticket_applies_status_on_create_transition() {
        let jira = FakeJiraClient::new()
            .with_create_issue_result(issue("PROJ-9"))
            .with_transitions(
                "PROJ-9",
                vec![transition("11", "Start Progress", "In Progress")],
            );
        let cfg = Config {
            status_on_create: Some("In Progress".to_string()),
            ..config()
        };
        let ctx = create_ctx(&jira, &cfg);

        let outcome = create_ticket(&ctx, "Add the widget", None).expect("should succeed");

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
        let cfg = Config {
            status_on_create: Some("In Progress".to_string()),
            ..config()
        };
        let ctx = create_ctx(&jira, &cfg);

        let outcome = create_ticket(&ctx, "Add the widget", None)
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
    fn create_ticket_no_status_on_create_configured_never_transitions() {
        let jira = FakeJiraClient::new().with_create_issue_result(issue("PROJ-9"));
        let cfg = config();
        let ctx = create_ctx(&jira, &cfg);

        let outcome = create_ticket(&ctx, "Add the widget", None).expect("should succeed");

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
}
