//! `tm ready` and `tm ready <KEY>`.
//!
//! Both forms classify a ticket as ready, **stackable**, or blocked via
//! [`crate::blocker_stacking::decide`] — the same decision table
//! [`crate::work::run::resolve_blocker_stacking`] uses to actually cut a
//! stacked branch, so none of the three can disagree.
//!
//! `tm ready` (no key, [`list`] / [`list_json`]) classifies every candidate
//! from [`crate::ticketing::ready_candidates`] (the current user's "To Do"
//! tickets) and reports each with its decision: ready, stackable on a
//! branch, or blocked with a reason. `tm ready <KEY>` ([`check`]) checks one
//! specific ticket (any assignee, any status). Ready
//! and stackable are both `Ok(`[`ReadyOutcome`]`)`, distinguished so `main.rs`
//! can give each its own exit code (see [`ReadyOutcome`]'s doc comment);
//! blocked is [`ReadyCliError::NotReady`], turned into a non-zero exit by
//! `main.rs`'s existing error path without special-casing `tm ready` for
//! that case. A ticket that would be ready or stackable but fails the
//! audit gate ([`AuditGate`], GitHub issue #112: no passing `tm ticket
//! audit` verdict in the local runs DB) is "needs audit" in the list and
//! [`ReadyCliError::NotAudited`] from [`check`].
//!
//! Both forms also carry a best-effort, ADVISORY annotation of GitHub bot
//! review findings (see [`crate::github::bot_findings`]) on a ready ticket's
//! associated pull request, if any. This is purely informational: it never
//! hides a ticket, changes an exit code, or turns a `gh` failure into an
//! error, since a bot false positive must not freeze a ticket. See
//! [`bot_finding_annotations`] and [`print_bot_finding_note`] for the lookup.

use std::collections::HashMap;
use std::io::Write;

use serde::Serialize;
use thiserror::Error;

use crate::blocker_stacking::{self, StackDecision};
use crate::github::bot_findings::count_bot_findings;
use crate::github::gh_cli::{GhCli, GhError, PrSummary};
use crate::github::pr::{PrInfo, find_issue_key};
use crate::ticketing::provider::TicketProvider;
use crate::ticketing::types::Issue;
use crate::ticketing::{TicketingError, open_blockers, ready_candidates};

/// Dependencies `tm ready`'s bot-findings annotation needs, alongside the
/// ticket provider, bundled to keep `list`/`check`'s arity within this project's
/// convention (see `TicketingContext`).
pub struct ReadyContext<'a> {
    /// Ticket provider used for the readiness query/check itself.
    pub jira: &'a dyn TicketProvider,
    /// `gh` client used for the best-effort bot-findings annotation.
    pub gh: &'a dyn GhCli,
    /// Bot logins configured as [`crate::config::Config::review_bots`].
    pub review_bots: &'a [String],
    /// Each ticket's live lane run (see [`blocker_stacking::in_flight_lanes`]):
    /// a no-PR blocker with one reads as waiting rather than blocked.
    pub in_flight: blocker_stacking::InFlight,
    /// The audit gate (GitHub issue #112): what a ticket with no blockers
    /// still needs before it counts as ready or stackable.
    pub audit: AuditGate<'a>,
}

/// The verdict `tm ticket audit --record` stores for a ticket that passes
/// [`AuditGate`] (see [`crate::cli::AuditVerdict::Ready`]).
pub const PASSING_AUDIT_VERDICT: &str = "ready";

/// `tm ready`'s audit gate (GitHub issue #112): with the gate on, a ticket
/// is only ready or stackable once its latest recorded audit verdict is
/// [`PASSING_AUDIT_VERDICT`]. Audits live in the local runs DB, so the gate
/// is per-machine.
pub enum AuditGate<'a> {
    /// `[ready] require_audit = false`: blockers alone decide.
    Off,
    /// Read each ticket's latest audit for `scope` from `store`.
    Store {
        /// The runs DB holding `ticket_audits`.
        store: &'a crate::runs::RunStore,
        /// [`crate::config::BackendIdentity::scope`] of this repo.
        scope: String,
    },
    /// The gate is on but the runs DB couldn't be opened (the error text):
    /// nothing passes. Fails closed, since an unattended dispatcher acts on
    /// whatever `tm ready` returns.
    Unavailable(String),
}

impl AuditGate<'_> {
    /// Why `key` fails the gate, or `None` if it passes. A store read error
    /// fails closed, like [`AuditGate::Unavailable`].
    fn needs_audit(&self, key: &str) -> Option<String> {
        match self {
            AuditGate::Off => None,
            AuditGate::Unavailable(err) => Some(format!("audit store unavailable: {err}")),
            AuditGate::Store { store, scope } => {
                match store.latest_audit_for_ticket(Some(scope), key) {
                    Ok(Some(audit)) if audit.verdict == PASSING_AUDIT_VERDICT => None,
                    Ok(Some(audit)) => Some(format!("last audit was {}", audit.verdict)),
                    Ok(None) => Some("not audited".to_string()),
                    Err(err) => Some(format!("could not read audit: {err}")),
                }
            }
        }
    }

    /// [`check`]'s gate: `Err(NotAudited)` if `key` fails it.
    fn require(&self, key: &str) -> Result<(), ReadyCliError> {
        match self.needs_audit(key) {
            None => Ok(()),
            Some(reason) => Err(ReadyCliError::NotAudited {
                key: key.to_string(),
                reason,
            }),
        }
    }
}

/// Errors surfaced by `tm ready`.
#[derive(Debug, Error)]
pub enum ReadyCliError {
    /// `key` didn't normalize to a valid Jira issue key shape.
    #[error("invalid ticket key `{key}`; expected a Jira key like PROJ-123")]
    InvalidKey {
        /// The key as originally passed on the command line.
        key: String,
    },

    /// `tm ready <KEY>` found `key` has at least one open blocker.
    ///
    /// Carries the full blocker list pre-formatted in `blockers` (one line
    /// per open blocker, `"  OTHER-KEY (status): summary"`) so the `Display`
    /// output the user sees, and that a script capturing stderr sees, names
    /// every blocker without a second lookup.
    #[error("{key} is blocked by:\n{blockers}")]
    NotReady {
        /// The issue key that was checked.
        key: String,
        /// Pre-formatted, newline-joined list of open blocker lines.
        blockers: String,
    },

    /// `tm ready <KEY>` found `key` would be ready or stackable, but it fails
    /// the [`AuditGate`]: never audited, a latest verdict other than
    /// [`PASSING_AUDIT_VERDICT`], or an unreadable audit store. `main.rs`
    /// gives this its own exit code.
    #[error("{key} needs audit ({reason}); record one with `tm ticket audit {key} --record ready`")]
    NotAudited {
        /// The issue key that was checked.
        key: String,
        /// Why it fails the gate, e.g. `not audited`.
        reason: String,
    },

    /// A Jira API call failed.
    #[error(transparent)]
    Ticketing(#[from] TicketingError),

    /// Writing output failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// `tm ready <KEY>` couldn't resolve `key`'s blocker PRs because `gh`
    /// itself reported a **permanent** failure (see
    /// [`crate::github::gh_cli::GhError::is_permanent`]) — a bug in how `tm`
    /// calls `gh`, not an environmental blip. Unlike a transient failure
    /// (which degrades quietly to [`crate::ticketing::open_blockers`]'s
    /// Jira-only answer, see [`check`]), this is surfaced loudly rather than
    /// swallowed: a permanent error means blocker/stack resolution never
    /// actually ran and never will until the code is fixed, so silently
    /// falling back would misreport a ticket's real stackability — the exact
    /// incident `resolve_blocker_stacking`'s identical distinction exists
    /// for.
    #[error(
        "bug in tm itself while resolving blockers for {key}: {source} (this will not resolve on retry)"
    )]
    GhBug {
        /// The issue key that was checked.
        key: String,
        /// The underlying permanent `gh` error.
        #[source]
        source: GhError,
    },
}

/// Normalize `key` the same way `tm ticket` does, mapping the shared
/// [`crate::cli::ticket::TicketCliError::InvalidKey`] into this module's own
/// error type rather than duplicating the regex.
fn normalize(key: &str) -> Result<String, ReadyCliError> {
    super::ticket::normalize_key(key).map_err(|err| match err {
        super::ticket::TicketCliError::InvalidKey { key } => ReadyCliError::InvalidKey { key },
        other => unreachable!("normalize_key only ever returns InvalidKey, got {other:?}"),
    })
}

/// `tm ready` (no key): print every candidate assigned to the current user
/// (see [`ready_candidates`]) with its [`ListDecision`], grouped as ready,
/// then stackable, then waiting, then blocked, each group in rank order:
///
/// - ready: `KEY  Summary`
/// - stackable: `KEY  Summary  [stackable on <branch> — blocked by
///   <BLOCKER>, PR #<N> open]`
/// - needs audit (would be ready or stackable, but fails
///   [`ReadyContext::audit`]): `KEY  Summary  [needs audit: <reason>]`, e.g.
///   `[needs audit: not audited]`
/// - waiting: `KEY  Summary  [waiting on <BLOCKER> (<run status>)]`
/// - blocked: `KEY  Summary  [<reason>]`, e.g. `[blocked by PROJ-9 (In
///   Progress), no PR yet]`
///
/// Any row whose ticket blocks other candidates (see
/// [`blocker_stacking::unblock_counts`]) ends with `  [unblocks N]`.
///
/// Prints `No ready tickets.` ahead of any other rows if nothing is ready
/// or stackable. Any warnings (see [`list_json`]) print first as `warning:
/// ...` lines. Always exits 0.
///
/// Each ready line also carries a best-effort, advisory bot-findings
/// annotation: when a ready ticket has an open PR (matched by title) with
/// unresolved bot review findings, the line becomes `KEY  Summary  [N
/// unresolved bot findings]` (singular "finding" when `N == 1`). Zero
/// unresolved or no matching PR leaves the line unchanged. See
/// [`bot_finding_annotations`] for how that lookup degrades if `gh` fails.
pub fn list(ctx: &ReadyContext, out: &mut dyn Write) -> Result<(), ReadyCliError> {
    let report = list_report(ctx)?;

    for warning in &report.warnings {
        writeln!(out, "warning: {warning}")?;
    }

    let any_workable = report.tickets.iter().any(|t| {
        matches!(
            t.decision,
            ListDecision::Ready | ListDecision::Stackable { .. }
        )
    });
    if !any_workable {
        writeln!(out, "No ready tickets.")?;
    }

    for group in 0..5 {
        for ticket in report.tickets.iter().filter(|t| list_group(t) == group) {
            write!(out, "{}  {}", ticket.key, ticket.summary)?;
            if let Some(note) = list_note(ticket) {
                write!(out, "  [{note}]")?;
            }
            if ticket.unblocks > 0 {
                write!(out, "  [unblocks {}]", ticket.unblocks)?;
            }
            writeln!(out)?;
        }
    }

    Ok(())
}

/// [`list`]'s print order: ready, stackable, needs audit, waiting, then
/// blocked.
fn list_group(ticket: &ListEntry) -> u8 {
    match ticket.decision {
        ListDecision::Ready => 0,
        ListDecision::Stackable { .. } => 1,
        ListDecision::NeedsAudit { .. } => 2,
        ListDecision::Waiting { .. } => 3,
        ListDecision::Blocked { .. } => 4,
    }
}

/// The bracketed note [`list`] prints after `ticket`'s summary, if any.
fn list_note(ticket: &ListEntry) -> Option<String> {
    match &ticket.decision {
        ListDecision::Ready => ticket.unresolved_bot_findings.map(|n| {
            let noun = if n == 1 { "finding" } else { "findings" };
            format!("{n} unresolved bot {noun}")
        }),
        ListDecision::Stackable {
            base,
            blocker_key,
            pr_number,
        } => Some(format!(
            "stackable on {base} — blocked by {blocker_key}, PR #{pr_number} open"
        )),
        ListDecision::NeedsAudit { reason } => Some(format!("needs audit: {reason}")),
        ListDecision::Waiting { reason, .. } | ListDecision::Blocked { reason, .. } => {
            Some(reason.clone())
        }
    }
}

/// `tm ready --json`: the same classification as [`list`], as one JSON
/// object on `out`:
///
/// ```json
/// {
///   "tickets": [
///     {"key": "AX-1", "summary": "...", "decision": "ready",
///      "unresolved_bot_findings": 2, "unblocks": 1},
///     {"key": "AX-409", "summary": "...", "decision": "stackable",
///      "base": "origin/jowi-dev/ax-408-...", "blocker_key": "AX-408",
///      "pr_number": 490, "unblocks": 0},
///     {"key": "AX-5", "summary": "...", "decision": "needs_audit",
///      "reason": "not audited", "unblocks": 0},
///     {"key": "AX-3", "summary": "...", "decision": "waiting",
///      "blocker_key": "AX-7", "run_id": 12, "run_status": "running",
///      "reason": "waiting on AX-7 (running)", "unblocks": 0},
///     {"key": "AX-2", "summary": "...", "decision": "blocked",
///      "blocker_keys": ["AX-9"], "reason": "blocked by AX-9 (To Do), no PR yet",
///      "unblocks": 0}
///   ],
///   "warnings": ["could not check stackability: ...",
///                "blocker cycle among AX-5, AX-6 — ..."]
/// }
/// ```
///
/// `tickets` is in rank order (not grouped). `unresolved_bot_findings` is
/// present only on a ready ticket with at least one; `unblocks` is always
/// present. `warnings` carries what [`list`] would print as `warning:`
/// lines, so stdout stays valid JSON when a `gh` lookup degrades.
pub fn list_json(ctx: &ReadyContext, out: &mut dyn Write) -> Result<(), ReadyCliError> {
    let report = list_report(ctx)?;
    serde_json::to_writer_pretty(&mut *out, &report).map_err(std::io::Error::from)?;
    writeln!(out)?;
    Ok(())
}

/// The autopilot dispatcher's view of `tm ready` (GitHub issue #115): the
/// same classification as [`list`], audit gate included, in rank order.
/// Warnings are dropped, since each one already degrades the affected
/// tickets to blocked rather than ready.
pub fn dispatch_candidates(
    ctx: &ReadyContext,
) -> Result<Vec<crate::work::dispatch::Candidate>, ReadyCliError> {
    Ok(list_report(ctx)?
        .tickets
        .into_iter()
        .map(|entry| crate::work::dispatch::Candidate {
            key: entry.key,
            decision: entry.decision,
            unblocks: entry.unblocks,
        })
        .collect())
}

/// A `tm ready` (no key) candidate's decision:[`blocker_stacking::decide`]'s
/// verdict, shaped for display. Serialized as the `decision` tag of a
/// [`list_json`] ticket, with the variant's fields alongside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum ListDecision {
    /// No unmerged direct blockers.
    Ready,
    /// Exactly one unmerged direct blocker, with an open PR to stack on.
    Stackable {
        /// The branch to stack on, `origin/<blocker's PR head branch>`.
        base: String,
        /// The blocker's issue key.
        blocker_key: String,
        /// The blocker's open PR number.
        pr_number: u64,
    },
    /// Would be ready or stackable, but fails the [`AuditGate`].
    NeedsAudit {
        /// Why, e.g. `not audited` or `last audit was needs-work`.
        reason: String,
    },
    /// One unmerged blocker with no PR yet, but a live lane run working it:
    /// re-check later rather than dispatch the blocker.
    Waiting {
        /// The blocker's issue key.
        blocker_key: String,
        /// The blocker's live lane run.
        run_id: i64,
        /// That run's status: `running`, `queued`, or `hibernated`.
        run_status: String,
        /// Human-readable explanation, e.g. `waiting on PROJ-9 (running)`.
        reason: String,
    },
    /// Nothing to stack on: one unmerged blocker with no PR, two or more
    /// unmerged blockers, or (when the PR lookup failed) any blocker still
    /// open in the tracker.
    Blocked {
        /// Every blocker still in the way.
        blocker_keys: Vec<String>,
        /// Human-readable explanation, e.g. `blocked by PROJ-9 (In
        /// Progress), no PR yet`.
        reason: String,
    },
}

/// One [`list_json`] ticket.
#[derive(Debug, Serialize)]
struct ListEntry {
    key: String,
    summary: String,
    #[serde(flatten)]
    decision: ListDecision,
    #[serde(skip_serializing_if = "Option::is_none")]
    unresolved_bot_findings: Option<usize>,
    /// How many other candidates this one blocks, transitively.
    unblocks: usize,
}

/// What both [`list`] and [`list_json`] render.
#[derive(Debug, Serialize)]
struct ListReport {
    tickets: Vec<ListEntry>,
    warnings: Vec<String>,
}

/// Classify every [`ready_candidates`] entry for [`list`]/[`list_json`].
///
/// [`GhCli::pr_list_all`] is called at most once per invocation, and only if
/// some candidate has a direct blocker. If it fails, a `could not check
/// stackability` warning is recorded (permanent errors flagged via
/// [`crate::github::gh_cli::permanence_note`]) and candidates fall back to
/// [`open_blockers`]' tracker-status answer, as [`check`]'s transient
/// fallback does: a `gh` hiccup must never mislabel a blocked ticket as
/// pickable, and failing the whole listing would be disproportionate.
fn list_report(ctx: &ReadyContext) -> Result<ListReport, ReadyCliError> {
    let candidates = ready_candidates(ctx.jira)?;
    let mut warnings = Vec::new();

    let prs = if candidates
        .iter()
        .any(|issue| !blocker_stacking::direct_blockers(issue).is_empty())
    {
        let cwd = std::env::current_dir()?;
        match ctx.gh.pr_list_all(&cwd) {
            Ok(prs) => Some(prs),
            Err(err) => {
                let note = crate::github::gh_cli::permanence_note(&err);
                warnings.push(format!("could not check stackability: {err}{note}"));
                None
            }
        }
    } else {
        None
    };

    let unblocks = blocker_stacking::unblock_counts(&candidates);
    if !unblocks.cycle_members.is_empty() {
        let members: Vec<&str> = unblocks.cycle_members.iter().map(String::as_str).collect();
        warnings.push(format!(
            "blocker cycle among {} — none of them can become ready until it is broken",
            members.join(", ")
        ));
    }

    let decisions: Vec<ListDecision> = candidates
        .iter()
        .map(|issue| {
            let decision = list_decision(issue, prs.as_deref(), &ctx.in_flight);
            match decision {
                ListDecision::Ready | ListDecision::Stackable { .. } => {
                    match ctx.audit.needs_audit(&issue.key) {
                        Some(reason) => ListDecision::NeedsAudit { reason },
                        None => decision,
                    }
                }
                _ => decision,
            }
        })
        .collect();

    let ready: Vec<Issue> = candidates
        .iter()
        .zip(&decisions)
        .filter(|(_, decision)| **decision == ListDecision::Ready)
        .map(|(issue, _)| issue.clone())
        .collect();
    let annotations = if ready.is_empty() {
        HashMap::new()
    } else {
        bot_finding_annotations(ctx, &ready, &mut warnings)?
    };

    let tickets = candidates
        .into_iter()
        .zip(decisions)
        .map(|(issue, decision)| ListEntry {
            unresolved_bot_findings: annotations.get(&issue.key).copied(),
            unblocks: unblocks.counts.get(&issue.key).copied().unwrap_or(0),
            key: issue.key,
            summary: issue.fields.summary,
            decision,
        })
        .collect();

    Ok(ListReport { tickets, warnings })
}

/// Classify `issue` via [`blocker_stacking::decide`]. `prs` is the blockers'
/// repo PR list, or `None` when that lookup failed (or was skipped because no
/// candidate had a blocker); a ticket with no direct blockers is ready
/// without consulting it, matching [`check`].
fn list_decision(
    issue: &Issue,
    prs: Option<&[PrSummary]>,
    in_flight: &blocker_stacking::InFlight,
) -> ListDecision {
    if blocker_stacking::direct_blockers(issue).is_empty() {
        return ListDecision::Ready;
    }
    let Some(prs) = prs else {
        let open = open_blockers(issue);
        if open.is_empty() {
            return ListDecision::Ready;
        }
        let named: Vec<String> = open
            .iter()
            .map(|b| format!("{} ({})", b.key, b.fields.status.name))
            .collect();
        return ListDecision::Blocked {
            blocker_keys: open.iter().map(|b| b.key.clone()).collect(),
            reason: format!("blocked by {} — blocker PRs unresolved", named.join(", ")),
        };
    };

    match blocker_stacking::decide(
        blocker_stacking::unmerged_direct_blockers(issue, prs, &|_| false),
        in_flight,
    ) {
        StackDecision::Ready => ListDecision::Ready,
        StackDecision::Stackable {
            blocker_key,
            pr_number,
            head_ref_name,
        } => ListDecision::Stackable {
            base: format!("origin/{head_ref_name}"),
            blocker_key,
            pr_number,
        },
        StackDecision::WaitingOnInFlight { blocker, run } => ListDecision::Waiting {
            reason: format!("waiting on {} ({})", blocker.key, run.status.as_str()),
            blocker_key: blocker.key,
            run_id: run.run_id,
            run_status: run.status.as_str().to_string(),
        },
        StackDecision::BlockedNoPr { blocker } => ListDecision::Blocked {
            reason: format!(
                "blocked by {} ({}), no PR yet",
                blocker.key, blocker.status_name
            ),
            blocker_keys: vec![blocker.key],
        },
        StackDecision::BlockedMultiple { blockers } => ListDecision::Blocked {
            reason: format!(
                "blocked by {} — can't stack on more than one",
                blockers
                    .iter()
                    .map(blocker_stacking::format_unmerged_blocker)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            blocker_keys: blockers.into_iter().map(|b| b.key).collect(),
        },
    }
}

/// Outcome of `tm ready <KEY>` on the non-blocked path: distinguishes plain
/// [`ReadyOutcome::Ready`] from [`ReadyOutcome::Stackable`] so `main.rs` can
/// give each its own exit code — the whole point of surfacing this state at
/// all is so an autonomous agent can branch on it without parsing stdout.
/// See `main.rs`'s `run_ready_check` for the exit-code mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadyOutcome {
    /// No unmerged direct blockers.
    Ready,
    /// Exactly one unmerged direct blocker, with an open PR to stack on —
    /// already printed as `stackable on <branch> (blocked by <KEY>, PR
    /// #<N> open)`.
    Stackable,
}

/// `tm ready <KEY>`: check whether `key` (any assignee, any status) is ready
/// to pick up, stackable, or blocked.
///
/// The decision — same rule [`crate::work::run::resolve_blocker_stacking`]
/// uses to actually cut a stacked branch, via
/// [`crate::blocker_stacking::decide`] — is:
/// - No unmerged direct `Blocks` blocker → prints `KEY is ready (<status>)`,
///   returns `Ok(ReadyOutcome::Ready)`.
/// - Exactly one, with an open PR → prints `KEY is stackable on
///   <branch> (blocked by <BLOCKER>, PR #<N> open)`, returns
///   `Ok(ReadyOutcome::Stackable)`.
/// - Exactly one, with no PR yet (whether waiting on a live lane run in
///   [`ReadyContext::in_flight`] or not), or two or more → returns
///   [`ReadyCliError::NotReady`], whose `Display` prints a `KEY is blocked
///   by:` header followed by one line per unmerged blocker — `main.rs`'s
///   existing error path turns this into a non-zero exit.
///
/// Resolving the decision needs `gh` (for each blocker's PR state, via
/// [`GhCli::pr_list_all`]), fetched only when `key` actually has a direct
/// blocker — a ticket with none never shells out. A **transient** `gh`
/// failure degrades quietly to [`crate::ticketing::open_blockers`]'s
/// Jira-status-only answer (today's pre-stacking behavior), printing a
/// `warning: could not resolve blocker PRs for KEY (...)  — falling back to
/// Jira-only readiness check` line first. A **permanent** `gh` failure (see
/// [`crate::github::gh_cli::GhError::is_permanent`]) is not swallowed: it
/// returns [`ReadyCliError::GhBug`], since silently degrading would
/// misreport a ticket's real stackability rather than merely being stale —
/// the same distinction `resolve_blocker_stacking` draws.
///
/// On the ready path only (including the Jira-only fallback), also prints a
/// best-effort, advisory bot-findings note (`  note: N unresolved bot
/// findings on PR #<number>`, singular "finding" when `N == 1`) when `key`
/// has an open PR (matched by title) with unresolved bot review findings.
/// This never affects the return value, and neither the stackable nor the
/// blocked path performs this lookup. See [`print_bot_finding_note`] for how
/// the lookup itself degrades if `gh` fails.
pub fn check(
    ctx: &ReadyContext,
    key: &str,
    out: &mut dyn Write,
) -> Result<ReadyOutcome, ReadyCliError> {
    let normalized = normalize(key)?;
    let issue = ctx
        .jira
        .get_issue(&normalized)
        .map_err(TicketingError::Provider)?;
    let status_name = issue.fields.status.name.clone();

    if blocker_stacking::direct_blockers(&issue).is_empty() {
        ctx.audit.require(&normalized)?;
        writeln!(out, "{normalized} is ready ({status_name})")?;
        print_bot_finding_note(ctx, &normalized, out)?;
        return Ok(ReadyOutcome::Ready);
    }

    let cwd = std::env::current_dir()?;
    let prs = match ctx.gh.pr_list_all(&cwd) {
        Ok(prs) => prs,
        Err(err) if err.is_permanent() => {
            return Err(ReadyCliError::GhBug {
                key: normalized,
                source: err,
            });
        }
        Err(err) => {
            return jira_only_fallback(ctx, &normalized, &issue, &status_name, &err, out);
        }
    };

    let unmerged = blocker_stacking::unmerged_direct_blockers(&issue, &prs, &|_| false);
    match blocker_stacking::decide(unmerged, &ctx.in_flight) {
        StackDecision::Ready => {
            ctx.audit.require(&normalized)?;
            writeln!(out, "{normalized} is ready ({status_name})")?;
            print_bot_finding_note(ctx, &normalized, out)?;
            Ok(ReadyOutcome::Ready)
        }
        StackDecision::Stackable {
            blocker_key,
            pr_number,
            head_ref_name,
        } => {
            ctx.audit.require(&normalized)?;
            let base = format!("origin/{head_ref_name}");
            writeln!(
                out,
                "{normalized} is stackable on {base} (blocked by {blocker_key}, PR #{pr_number} open)"
            )?;
            Ok(ReadyOutcome::Stackable)
        }
        StackDecision::BlockedNoPr { blocker } => Err(ReadyCliError::NotReady {
            key: normalized,
            blockers: format!(
                "  {} ({}): {} — blocked, no PR found yet to stack on",
                blocker.key, blocker.status_name, blocker.summary
            ),
        }),
        StackDecision::WaitingOnInFlight { blocker, run } => Err(ReadyCliError::NotReady {
            key: normalized,
            blockers: format!(
                "  {} ({}): {} — waiting on lane run #{} ({}), no PR yet",
                blocker.key,
                blocker.status_name,
                blocker.summary,
                run.run_id,
                run.status.as_str()
            ),
        }),
        StackDecision::BlockedMultiple { blockers } => {
            let mut lines: Vec<String> = blockers
                .iter()
                .map(|b| {
                    let pr_note = match &b.open_pr {
                        Some((number, _)) => format!("PR #{number} open, unmerged"),
                        None => "no PR yet".to_string(),
                    };
                    format!("  {} ({}): {} — {pr_note}", b.key, b.status_name, b.summary)
                })
                .collect();
            lines.push(
                "  (two or more unmerged blockers — a single ticket can't stack on more than one)"
                    .to_string(),
            );
            Err(ReadyCliError::NotReady {
                key: normalized,
                blockers: lines.join("\n"),
            })
        }
    }
}

/// Fallback for [`check`] when [`GhCli::pr_list_all`] fails **transiently**:
/// today's pre-stacking, Jira-status-only readiness check (via
/// [`crate::ticketing::open_blockers`]), with no PR/stack awareness at all.
/// A network hiccup must never freeze or wrongly block a ticket Jira itself
/// has no problem with — mirrors `resolve_blocker_stacking`'s identical
/// transient-failure fallback.
fn jira_only_fallback(
    ctx: &ReadyContext,
    normalized: &str,
    issue: &Issue,
    status_name: &str,
    gh_err: &GhError,
    out: &mut dyn Write,
) -> Result<ReadyOutcome, ReadyCliError> {
    let note = crate::github::gh_cli::permanence_note(gh_err);
    writeln!(
        out,
        "warning: could not resolve blocker PRs for {normalized} ({gh_err}) — falling back to Jira-only readiness check{note}"
    )?;

    let blockers = open_blockers(issue);
    if blockers.is_empty() {
        ctx.audit.require(normalized)?;
        writeln!(out, "{normalized} is ready ({status_name})")?;
        print_bot_finding_note(ctx, normalized, out)?;
        return Ok(ReadyOutcome::Ready);
    }

    let blockers = blockers
        .iter()
        .map(|b| {
            format!(
                "  {} ({}): {}",
                b.key, b.fields.status.name, b.fields.summary
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    Err(ReadyCliError::NotReady {
        key: normalized.to_string(),
        blockers,
    })
}

/// For [`list_report`]: compute unresolved bot-finding counts for every ticket in
/// `tickets` that has a matching open PR, degrading to a recorded warning
/// (once) rather than an error if `gh` fails.
///
/// Calls `gh.pr_list()` once. If it fails, pushes
/// `could not check bot findings: {err}` onto `warnings` and returns an empty map —
/// the listing is then printed unannotated. Otherwise, for each ticket
/// matched to an open PR (by [`find_issue_key`] against the PR title), calls
/// `gh.pr_review_threads`; a failure there records the same warning
/// exactly once (not once per ticket) and simply omits that ticket (and any
/// ticket whose lookup fails afterward) from the returned map, without
/// aborting the remaining lookups.
fn bot_finding_annotations(
    ctx: &ReadyContext,
    tickets: &[Issue],
    warnings: &mut Vec<String>,
) -> Result<HashMap<String, usize>, ReadyCliError> {
    let mut annotations = HashMap::new();

    // `tm ready` is run from inside the repo, so its own process cwd is the
    // right `dir` to shell `gh` against — see `GhCli::pr_list`'s doc
    // comment on why this argument exists at all (`tm pr watch`'s detached,
    // not-necessarily-in-repo case, not this one).
    let cwd = std::env::current_dir()?;

    let prs = match ctx.gh.pr_list(&cwd) {
        Ok(prs) => prs,
        Err(err) => {
            let note = crate::github::gh_cli::permanence_note(&err);
            warnings.push(format!("could not check bot findings: {err}{note}"));
            return Ok(annotations);
        }
    };

    let mut warned = false;
    for issue in tickets {
        let Some(number) = matching_pr_number(&prs, &issue.key, ctx.jira) else {
            continue;
        };
        match ctx.gh.pr_review_threads(&cwd, number) {
            Ok(threads) => {
                let counts = count_bot_findings(&threads, ctx.review_bots);
                if counts.unresolved > 0 {
                    annotations.insert(issue.key.clone(), counts.unresolved);
                }
            }
            Err(err) => {
                if !warned {
                    let note = crate::github::gh_cli::permanence_note(&err);
                    warnings.push(format!("could not check bot findings: {err}{note}"));
                    warned = true;
                }
            }
        }
    }

    Ok(annotations)
}

/// For `check`: print a `  note: N unresolved bot findings on PR #<number>`
/// line (singular "finding" when `N == 1`) if `key` has a matching open PR
/// with unresolved bot review findings.
///
/// Degrades the same way as [`bot_finding_annotations`]: a `pr_list` or
/// `pr_review_threads` failure prints
/// `warning: could not check bot findings: {err}` instead, never an error.
fn print_bot_finding_note(
    ctx: &ReadyContext,
    key: &str,
    out: &mut dyn Write,
) -> Result<(), ReadyCliError> {
    let cwd = std::env::current_dir()?;

    let prs = match ctx.gh.pr_list(&cwd) {
        Ok(prs) => prs,
        Err(err) => {
            let note = crate::github::gh_cli::permanence_note(&err);
            writeln!(out, "warning: could not check bot findings: {err}{note}")?;
            return Ok(());
        }
    };

    let Some(number) = matching_pr_number(&prs, key, ctx.jira) else {
        return Ok(());
    };

    match ctx.gh.pr_review_threads(&cwd, number) {
        Ok(threads) => {
            let counts = count_bot_findings(&threads, ctx.review_bots);
            if counts.unresolved > 0 {
                let noun = if counts.unresolved == 1 {
                    "finding"
                } else {
                    "findings"
                };
                writeln!(
                    out,
                    "  note: {} unresolved bot {noun} on PR #{number}",
                    counts.unresolved
                )?;
            }
        }
        Err(err) => {
            let note = crate::github::gh_cli::permanence_note(&err);
            writeln!(out, "warning: could not check bot findings: {err}{note}")?;
        }
    }

    Ok(())
}

/// Find the number of the open PR (from `gh pr list`) resolving to `key`,
/// reusing [`find_issue_key`] — the same title/branch/body extraction `tm pr
/// status` uses, keyed to `jira`'s notion of a valid ticket key — rather
/// than re-implementing key parsing.
fn matching_pr_number(prs: &[PrInfo], key: &str, jira: &dyn TicketProvider) -> Option<u64> {
    prs.iter()
        .find(|pr| find_issue_key(pr, &|token| jira.is_ticket_key(token)).as_deref() == Some(key))
        .map(|pr| pr.number)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::bot_findings::ReviewThread;
    use crate::github::gh_cli::{FakeGhCli, GhError};
    use crate::jira::fake::FakeJiraClient;
    use crate::ticketing::types::{
        Issue, IssueFields, IssueLink, IssueLinkType, LinkedIssue, LinkedIssueFields, SearchResult,
        Status, StatusCategory,
    };

    /// The default `review_bots` config: `["cursor[bot]"]`.
    fn cursor_bot() -> Vec<String> {
        vec!["cursor[bot]".to_string()]
    }

    fn ready_ctx<'a>(
        jira: &'a FakeJiraClient,
        gh: &'a FakeGhCli,
        review_bots: &'a [String],
    ) -> ReadyContext<'a> {
        ReadyContext {
            jira,
            gh,
            review_bots,
            in_flight: blocker_stacking::InFlight::new(),
            audit: AuditGate::Off,
        }
    }

    fn pr_all_summary(
        number: u64,
        head_ref_name: &str,
        lifecycle: crate::github::gh_cli::PrLifecycle,
    ) -> crate::github::gh_cli::PrSummary {
        crate::github::gh_cli::PrSummary {
            number,
            head_ref_name: head_ref_name.to_string(),
            head_ref_oid: String::new(),
            lifecycle,
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    fn pr_summary(number: u64, title: &str) -> PrInfo {
        PrInfo {
            number,
            url: String::new(),
            title: title.to_string(),
            body: String::new(),
            head_ref_name: String::new(),
            base_ref_name: "main".to_string(),
        }
    }

    fn review_thread(is_resolved: bool, author_login: &str) -> ReviewThread {
        ReviewThread {
            is_resolved,
            author_login: Some(author_login.to_string()),
        }
    }

    fn issue(key: &str) -> Issue {
        Issue {
            key: key.to_string(),
            fields: IssueFields {
                summary: format!("Summary for {key}"),
                status: Status {
                    name: "To Do".to_string(),
                    status_category: StatusCategory {
                        key: "new".to_string(),
                    },
                },
                description: None,
                assignee: None,
                issue_links: vec![],
                parent: None,
                labels: Vec::new(),
                issue_type: None,
            },
        }
    }

    fn blocks_link_type() -> IssueLinkType {
        IssueLinkType {
            name: "Blocks".to_string(),
            inward: "is blocked by".to_string(),
            outward: "blocks".to_string(),
        }
    }

    fn linked_issue(key: &str, status_name: &str, status_category_key: &str) -> LinkedIssue {
        LinkedIssue {
            key: key.to_string(),
            fields: LinkedIssueFields {
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

    fn search_result(issues: Vec<Issue>) -> SearchResult {
        SearchResult {
            issues,
            next_page_token: None,
        }
    }

    #[test]
    fn list_happy_path_prints_key_and_summary_per_line() {
        let jira = FakeJiraClient::new()
            .with_search_result(search_result(vec![issue("PROJ-1"), issue("PROJ-2")]));
        let gh = FakeGhCli::new();
        let bots = cursor_bot();
        let mut out = Vec::new();

        list(&ready_ctx(&jira, &gh, &bots), &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(
            output,
            "PROJ-1  Summary for PROJ-1\nPROJ-2  Summary for PROJ-2\n"
        );
    }

    #[test]
    fn list_with_no_candidates_prints_no_ready_tickets() {
        let jira = FakeJiraClient::new().with_search_result(search_result(vec![]));
        let gh = FakeGhCli::new();
        let bots = cursor_bot();
        let mut out = Vec::new();

        list(&ready_ctx(&jira, &gh, &bots), &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(output, "No ready tickets.\n");
    }

    /// `issue(key)` with one direct `Blocks` link per `(blocker_key,
    /// status_name, status_category_key)`.
    fn blocked_issue(key: &str, blockers: &[(&str, &str, &str)]) -> Issue {
        let mut i = issue(key);
        i.fields.issue_links = blockers
            .iter()
            .enumerate()
            .map(|(n, (blocker, status, category))| IssueLink {
                id: format!("1000{n}"),
                link_type: blocks_link_type(),
                inward_issue: Some(linked_issue(blocker, status, category)),
                outward_issue: None,
            })
            .collect();
        i
    }

    /// `key`'s blocker has a live lane run `run_id` in `status`.
    fn in_flight(
        key: &str,
        run_id: i64,
        status: crate::runs::RunStatus,
    ) -> blocker_stacking::InFlight {
        blocker_stacking::InFlight::from([(
            key.to_string(),
            blocker_stacking::InFlightRun { run_id, status },
        )])
    }

    #[test]
    fn list_shows_a_blocker_with_a_live_lane_run_as_waiting() {
        let jira = FakeJiraClient::new().with_search_result(search_result(vec![
            blocked_issue("PROJ-2", &[("PROJ-9", "In Progress", "indeterminate")]),
            blocked_issue("PROJ-3", &[("PROJ-8", "In Progress", "indeterminate")]),
        ]));
        let gh = FakeGhCli::new();
        let bots = cursor_bot();
        let ctx = ReadyContext {
            in_flight: in_flight("PROJ-9", 5, crate::runs::RunStatus::Running),
            ..ready_ctx(&jira, &gh, &bots)
        };
        let mut out = Vec::new();

        list(&ctx, &mut out).expect("should succeed");

        assert_eq!(
            String::from_utf8(out).unwrap(),
            "No ready tickets.\n\
             PROJ-2  Summary for PROJ-2  [waiting on PROJ-9 (running)]\n\
             PROJ-3  Summary for PROJ-3  [blocked by PROJ-8 (In Progress), no PR yet]\n"
        );
    }

    #[test]
    fn list_json_reports_a_waiting_candidate_with_its_run() {
        let jira = FakeJiraClient::new().with_search_result(search_result(vec![blocked_issue(
            "PROJ-2",
            &[("PROJ-9", "In Progress", "indeterminate")],
        )]));
        let gh = FakeGhCli::new();
        let bots = cursor_bot();
        let ctx = ReadyContext {
            in_flight: in_flight("PROJ-9", 5, crate::runs::RunStatus::Hibernated),
            ..ready_ctx(&jira, &gh, &bots)
        };
        let mut out = Vec::new();

        list_json(&ctx, &mut out).expect("should succeed");

        let value: serde_json::Value = serde_json::from_slice(&out).expect("valid JSON");
        assert_eq!(
            value["tickets"][0],
            serde_json::json!({
                "key": "PROJ-2",
                "summary": "Summary for PROJ-2",
                "decision": "waiting",
                "blocker_key": "PROJ-9",
                "run_id": 5,
                "run_status": "hibernated",
                "reason": "waiting on PROJ-9 (hibernated)",
                "unblocks": 0,
            })
        );
    }

    #[test]
    fn list_shows_how_many_candidates_each_ticket_unblocks() {
        // PROJ-1 blocks PROJ-2, which blocks PROJ-3: PROJ-1 frees two.
        let jira = FakeJiraClient::new().with_search_result(search_result(vec![
            issue("PROJ-1"),
            blocked_issue("PROJ-2", &[("PROJ-1", "To Do", "new")]),
            blocked_issue("PROJ-3", &[("PROJ-2", "To Do", "new")]),
        ]));
        let gh = FakeGhCli::new();
        let bots = cursor_bot();
        let mut out = Vec::new();

        list(&ready_ctx(&jira, &gh, &bots), &mut out).expect("should succeed");

        assert_eq!(
            String::from_utf8(out).unwrap(),
            "PROJ-1  Summary for PROJ-1  [unblocks 2]\n\
             PROJ-2  Summary for PROJ-2  [blocked by PROJ-1 (To Do), no PR yet]  [unblocks 1]\n\
             PROJ-3  Summary for PROJ-3  [blocked by PROJ-2 (To Do), no PR yet]\n"
        );
    }

    #[test]
    fn list_json_carries_unblock_counts_and_warns_about_a_cycle() {
        let jira = FakeJiraClient::new().with_search_result(search_result(vec![
            blocked_issue("PROJ-1", &[("PROJ-2", "To Do", "new")]),
            blocked_issue("PROJ-2", &[("PROJ-1", "To Do", "new")]),
            issue("PROJ-3"),
        ]));
        let gh = FakeGhCli::new();
        let bots = cursor_bot();
        let mut out = Vec::new();

        list_json(&ready_ctx(&jira, &gh, &bots), &mut out).expect("should succeed");

        let value: serde_json::Value = serde_json::from_slice(&out).expect("valid JSON");
        assert_eq!(value["tickets"][0]["unblocks"], 1);
        assert_eq!(value["tickets"][2]["unblocks"], 0);
        assert_eq!(
            value["warnings"],
            serde_json::json!([
                "blocker cycle among PROJ-1, PROJ-2 — none of them can become ready until it is broken"
            ])
        );
    }

    #[test]
    fn dispatch_candidates_carry_each_decision_and_unblock_count_in_rank_order() {
        let jira = FakeJiraClient::new().with_search_result(search_result(vec![
            issue("PROJ-1"),
            blocked_issue("PROJ-2", &[("PROJ-1", "To Do", "new")]),
        ]));
        let gh = FakeGhCli::new().with_pr_list_all(Ok(vec![]));
        let bots = cursor_bot();

        let candidates = dispatch_candidates(&ready_ctx(&jira, &gh, &bots)).expect("classified");

        assert_eq!(
            candidates,
            vec![
                crate::work::dispatch::Candidate {
                    key: "PROJ-1".to_string(),
                    decision: ListDecision::Ready,
                    unblocks: 1,
                },
                crate::work::dispatch::Candidate {
                    key: "PROJ-2".to_string(),
                    decision: ListDecision::Blocked {
                        blocker_keys: vec!["PROJ-1".to_string()],
                        reason: "blocked by PROJ-1 (To Do), no PR yet".to_string(),
                    },
                    unblocks: 0,
                },
            ]
        );
    }

    #[test]
    fn dispatch_candidates_apply_the_audit_gate() {
        let jira = FakeJiraClient::new().with_search_result(search_result(vec![issue("PROJ-1")]));
        let gh = FakeGhCli::new();
        let bots = cursor_bot();
        let ctx = ReadyContext {
            audit: AuditGate::Unavailable("no db".to_string()),
            ..ready_ctx(&jira, &gh, &bots)
        };

        let candidates = dispatch_candidates(&ctx).expect("classified");

        assert_eq!(
            candidates[0].decision,
            ListDecision::NeedsAudit {
                reason: "audit store unavailable: no db".to_string()
            }
        );
    }

    #[test]
    fn check_reports_a_blocker_with_a_live_lane_run_as_waiting() {
        let ticket = blocked_issue("AX-409", &[("AX-408", "In Progress", "indeterminate")]);
        let jira = FakeJiraClient::new().with_issue("AX-409", ticket);
        let gh = FakeGhCli::new().with_pr_list_all(Ok(vec![]));
        let bots = cursor_bot();
        let ctx = ReadyContext {
            in_flight: in_flight("AX-408", 12, crate::runs::RunStatus::Queued),
            ..ready_ctx(&jira, &gh, &bots)
        };
        let mut out = Vec::new();

        let err = check(&ctx, "AX-409", &mut out).expect_err("waiting is not ready");

        assert_eq!(
            err.to_string(),
            "AX-409 is blocked by:\n  AX-408 (In Progress): Summary for AX-408 — waiting on lane run #12 (queued), no PR yet"
        );
    }

    #[test]
    fn list_shows_a_blocked_candidate_with_its_reason() {
        let blocked = blocked_issue("PROJ-2", &[("PROJ-9", "In Progress", "indeterminate")]);
        let jira =
            FakeJiraClient::new().with_search_result(search_result(vec![issue("PROJ-1"), blocked]));
        let gh = FakeGhCli::new();
        let bots = cursor_bot();
        let mut out = Vec::new();

        list(&ready_ctx(&jira, &gh, &bots), &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(
            output,
            "PROJ-1  Summary for PROJ-1\n\
             PROJ-2  Summary for PROJ-2  [blocked by PROJ-9 (In Progress), no PR yet]\n"
        );
    }

    #[test]
    fn list_all_blocked_prints_no_ready_tickets_then_the_blocked_rows() {
        let blocked = blocked_issue("PROJ-1", &[("PROJ-9", "In Progress", "indeterminate")]);
        let jira = FakeJiraClient::new().with_search_result(search_result(vec![blocked]));
        let gh = FakeGhCli::new();
        let bots = cursor_bot();
        let mut out = Vec::new();

        list(&ready_ctx(&jira, &gh, &bots), &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(
            output,
            "No ready tickets.\n\
             PROJ-1  Summary for PROJ-1  [blocked by PROJ-9 (In Progress), no PR yet]\n"
        );
    }

    #[test]
    fn list_names_every_blocker_when_there_are_two_or_more() {
        let blocked = blocked_issue(
            "PROJ-1",
            &[
                ("PROJ-8", "In Review", "indeterminate"),
                ("PROJ-9", "To Do", "new"),
            ],
        );
        let jira = FakeJiraClient::new().with_search_result(search_result(vec![blocked]));
        let gh = FakeGhCli::new().with_pr_list_all(Ok(vec![pr_all_summary(
            3,
            "jowi-dev/proj-8-thing",
            crate::github::gh_cli::PrLifecycle::Open,
        )]));
        let bots = cursor_bot();
        let mut out = Vec::new();

        list(&ready_ctx(&jira, &gh, &bots), &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(
            output.contains(
                "PROJ-1  Summary for PROJ-1  [blocked by PROJ-8 (PR #3 open), PROJ-9 (no PR) \
                 — can't stack on more than one]\n"
            ),
            "{output}"
        );
    }

    #[test]
    fn list_clears_a_blocker_whose_pr_is_merged_even_if_its_status_lags() {
        let ticket = blocked_issue("PROJ-2", &[("PROJ-9", "In Review", "indeterminate")]);
        let jira = FakeJiraClient::new().with_search_result(search_result(vec![ticket]));
        let gh = FakeGhCli::new().with_pr_list_all(Ok(vec![pr_all_summary(
            7,
            "jowi-dev/proj-9-thing",
            crate::github::gh_cli::PrLifecycle::Merged,
        )]));
        let bots = cursor_bot();
        let mut out = Vec::new();

        list(&ready_ctx(&jira, &gh, &bots), &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(output, "PROJ-2  Summary for PROJ-2\n");
    }

    #[test]
    fn list_clears_a_blocker_that_is_done_in_the_tracker() {
        let ticket = blocked_issue("PROJ-2", &[("PROJ-9", "Done", "done")]);
        let jira = FakeJiraClient::new().with_search_result(search_result(vec![ticket]));
        let gh = FakeGhCli::new();
        let bots = cursor_bot();
        let mut out = Vec::new();

        list(&ready_ctx(&jira, &gh, &bots), &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(output, "PROJ-2  Summary for PROJ-2\n");
    }

    #[test]
    fn list_fetches_the_pr_list_once_however_many_candidates_have_blockers() {
        let jira = FakeJiraClient::new().with_search_result(search_result(vec![
            blocked_issue("PROJ-1", &[("PROJ-7", "In Progress", "indeterminate")]),
            blocked_issue("PROJ-2", &[("PROJ-8", "In Progress", "indeterminate")]),
            blocked_issue("PROJ-3", &[("PROJ-9", "In Progress", "indeterminate")]),
        ]));
        let gh = FakeGhCli::new();
        let bots = cursor_bot();
        let mut out = Vec::new();

        list(&ready_ctx(&jira, &gh, &bots), &mut out).expect("should succeed");

        assert_eq!(gh.pr_list_all_calls().len(), 1);
    }

    #[test]
    fn list_with_no_blocked_candidates_never_fetches_the_pr_list() {
        let jira = FakeJiraClient::new().with_search_result(search_result(vec![issue("PROJ-1")]));
        let gh = FakeGhCli::new();
        let bots = cursor_bot();
        let mut out = Vec::new();

        list(&ready_ctx(&jira, &gh, &bots), &mut out).expect("should succeed");

        assert!(gh.pr_list_all_calls().is_empty());
    }

    #[test]
    fn list_json_carries_each_candidates_decision_in_rank_order() {
        let jira = FakeJiraClient::new().with_search_result(search_result(vec![
            blocked_issue("AX-409", &[("AX-408", "Code Review", "indeterminate")]),
            issue("AX-1"),
            blocked_issue("AX-2", &[("AX-9", "To Do", "new")]),
        ]));
        let gh = FakeGhCli::new().with_pr_list_all(Ok(vec![pr_all_summary(
            490,
            "jowi-dev/ax-408-20260810-131418",
            crate::github::gh_cli::PrLifecycle::Open,
        )]));
        let bots = cursor_bot();
        let mut out = Vec::new();

        list_json(&ready_ctx(&jira, &gh, &bots), &mut out).expect("should succeed");

        let value: serde_json::Value = serde_json::from_slice(&out).expect("valid JSON");
        assert_eq!(
            value,
            serde_json::json!({
                "tickets": [
                    {
                        "key": "AX-409",
                        "summary": "Summary for AX-409",
                        "decision": "stackable",
                        "base": "origin/jowi-dev/ax-408-20260810-131418",
                        "blocker_key": "AX-408",
                        "pr_number": 490,
                        "unblocks": 0,
                    },
                    {
                        "key": "AX-1",
                        "summary": "Summary for AX-1",
                        "decision": "ready",
                        "unblocks": 0,
                    },
                    {
                        "key": "AX-2",
                        "summary": "Summary for AX-2",
                        "decision": "blocked",
                        "blocker_keys": ["AX-9"],
                        "reason": "blocked by AX-9 (To Do), no PR yet",
                        "unblocks": 0,
                    },
                ],
                "warnings": [],
            })
        );
    }

    #[test]
    fn list_json_reports_warnings_and_bot_findings_without_corrupting_stdout() {
        let jira = FakeJiraClient::new().with_search_result(search_result(vec![
            issue("PROJ-1"),
            blocked_issue("PROJ-2", &[("PROJ-9", "In Progress", "indeterminate")]),
        ]));
        let gh = FakeGhCli::new()
            .with_pr_list_all(Err(GhError::Command {
                command: "gh pr list".to_string(),
                exit_code: Some(1),
                stderr: "boom".to_string(),
            }))
            .with_pr_list(Ok(vec![pr_summary(42, "[PROJ-1] Fix the thing")]))
            .with_review_threads(42, Ok(vec![review_thread(false, "cursor")]));
        let bots = cursor_bot();
        let mut out = Vec::new();

        list_json(&ready_ctx(&jira, &gh, &bots), &mut out).expect("should succeed");

        let value: serde_json::Value = serde_json::from_slice(&out).expect("valid JSON");
        assert_eq!(value["tickets"][0]["unresolved_bot_findings"], 1);
        assert_eq!(value["tickets"][1]["decision"], "blocked");
        let warnings = value["warnings"].as_array().expect("warnings array");
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0]
                .as_str()
                .unwrap()
                .starts_with("could not check stackability")
        );
    }

    #[test]
    fn list_annotates_ticket_with_matched_pr_and_unresolved_bot_findings() {
        let jira = FakeJiraClient::new()
            .with_search_result(search_result(vec![issue("PROJ-1"), issue("PROJ-2")]));
        let gh = FakeGhCli::new()
            .with_pr_list(Ok(vec![
                pr_summary(42, "[PROJ-1] Fix the thing"),
                pr_summary(43, "[PROJ-2] Add the widget"),
            ]))
            .with_review_threads(
                42,
                Ok(vec![
                    review_thread(false, "cursor"),
                    review_thread(false, "cursor"),
                    review_thread(true, "cursor"),
                ]),
            )
            .with_review_threads(43, Ok(vec![review_thread(false, "cursor")]));
        let bots = cursor_bot();
        let mut out = Vec::new();

        list(&ready_ctx(&jira, &gh, &bots), &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(
            output,
            "PROJ-1  Summary for PROJ-1  [2 unresolved bot findings]\n\
             PROJ-2  Summary for PROJ-2  [1 unresolved bot finding]\n"
        );
    }

    #[test]
    fn list_leaves_line_unchanged_when_zero_unresolved_bot_findings() {
        let jira = FakeJiraClient::new().with_search_result(search_result(vec![issue("PROJ-1")]));
        let gh = FakeGhCli::new()
            .with_pr_list(Ok(vec![pr_summary(42, "[PROJ-1] Fix the thing")]))
            .with_review_threads(42, Ok(vec![review_thread(true, "cursor")]));
        let bots = cursor_bot();
        let mut out = Vec::new();

        list(&ready_ctx(&jira, &gh, &bots), &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(output, "PROJ-1  Summary for PROJ-1\n");
    }

    #[test]
    fn list_leaves_line_unchanged_when_no_matching_pr() {
        let jira = FakeJiraClient::new().with_search_result(search_result(vec![issue("PROJ-1")]));
        let gh = FakeGhCli::new().with_pr_list(Ok(vec![pr_summary(42, "Unrelated PR")]));
        let bots = cursor_bot();
        let mut out = Vec::new();

        list(&ready_ctx(&jira, &gh, &bots), &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(output, "PROJ-1  Summary for PROJ-1\n");
        assert!(!output.contains("warning"));
    }

    #[test]
    fn list_pr_list_failure_warns_once_and_prints_unannotated_list() {
        let jira = FakeJiraClient::new()
            .with_search_result(search_result(vec![issue("PROJ-1"), issue("PROJ-2")]));
        let gh = FakeGhCli::new().with_pr_list(Err(GhError::Command {
            command: "gh pr list".to_string(),
            exit_code: Some(1),
            stderr: "boom".to_string(),
        }));
        let bots = cursor_bot();
        let mut out = Vec::new();

        list(&ready_ctx(&jira, &gh, &bots), &mut out).expect("should still succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(
            output,
            "warning: could not check bot findings: `gh pr list` failed (exit Some(1)): boom\n\
             PROJ-1  Summary for PROJ-1\n\
             PROJ-2  Summary for PROJ-2\n"
        );
    }

    #[test]
    fn list_pr_list_permanent_failure_flags_the_warning_as_a_tm_bug() {
        // A permanent gh error deserves a louder warning than a plain
        // network hiccup — "surface it prominently", not a warning line
        // that reads identically for a bug that will never resolve itself
        // and a blip that will. `tm ready list` still degrades to an
        // unannotated listing either way (a best-effort side annotation,
        // not worth hard-failing the whole command over).
        let jira = FakeJiraClient::new().with_search_result(search_result(vec![issue("PROJ-1")]));
        let gh = FakeGhCli::new().with_pr_list(Err(GhError::Command {
            command: "gh pr list".to_string(),
            exit_code: Some(1),
            stderr: r#"unknown JSON field: "merged""#.to_string(),
        }));
        let bots = cursor_bot();
        let mut out = Vec::new();

        list(&ready_ctx(&jira, &gh, &bots), &mut out).expect("should still succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(
            output.contains("bug in tm itself"),
            "expected the warning to flag this as a permanent tm bug, got: {output:?}"
        );
    }

    #[test]
    fn list_review_threads_failure_for_one_ticket_warns_once_and_still_annotates_the_other() {
        let jira = FakeJiraClient::new()
            .with_search_result(search_result(vec![issue("PROJ-1"), issue("PROJ-2")]));
        let gh = FakeGhCli::new()
            .with_pr_list(Ok(vec![
                pr_summary(42, "[PROJ-1] Fix the thing"),
                pr_summary(43, "[PROJ-2] Add the widget"),
            ]))
            .with_review_threads(
                42,
                Err(GhError::Command {
                    command: "gh api graphql".to_string(),
                    exit_code: Some(1),
                    stderr: "boom".to_string(),
                }),
            )
            .with_review_threads(43, Ok(vec![review_thread(false, "cursor")]));
        let bots = cursor_bot();
        let mut out = Vec::new();

        list(&ready_ctx(&jira, &gh, &bots), &mut out).expect("should still succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(
            output.matches("warning:").count(),
            1,
            "expected exactly one warning line: {output}"
        );
        assert!(output.contains("PROJ-1  Summary for PROJ-1\n"));
        assert!(output.contains("PROJ-2  Summary for PROJ-2  [1 unresolved bot finding]\n"));
    }

    #[test]
    fn check_ready_ticket_prints_ready_message() {
        let jira = FakeJiraClient::new().with_issue("PROJ-1", issue("PROJ-1"));
        let gh = FakeGhCli::new();
        let bots = cursor_bot();
        let mut out = Vec::new();

        check(&ready_ctx(&jira, &gh, &bots), "proj-1", &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(output, "PROJ-1 is ready (To Do)\n");
    }

    #[test]
    fn check_ready_ticket_with_unresolved_bot_findings_prints_note() {
        let jira = FakeJiraClient::new().with_issue("PROJ-1", issue("PROJ-1"));
        let gh = FakeGhCli::new()
            .with_pr_list(Ok(vec![pr_summary(42, "[PROJ-1] Fix the thing")]))
            .with_review_threads(42, Ok(vec![review_thread(false, "cursor")]));
        let bots = cursor_bot();
        let mut out = Vec::new();

        check(&ready_ctx(&jira, &gh, &bots), "proj-1", &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(
            output,
            "PROJ-1 is ready (To Do)\n  note: 1 unresolved bot finding on PR #42\n"
        );
    }

    #[test]
    fn check_blocked_ticket_is_an_error_listing_unmerged_blockers_and_excluding_a_merged_one() {
        // PROJ-3's PR is merged, so it's satisfied regardless of its Jira
        // status still reading "Done" only in the fixture's naming — the
        // point being tested is that PR merge state, not Jira status,
        // decides satisfaction (see `crate::blocker_stacking`'s doc
        // comment). PROJ-2 has no PR at all, so it's the sole unmerged
        // blocker and the ticket is `BlockedNoPr`, not `BlockedMultiple`.
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
        let gh = FakeGhCli::new()
            .with_pr_list(Err(GhError::Command {
                command: "gh pr list".to_string(),
                exit_code: Some(1),
                stderr: "should never be called".to_string(),
            }))
            .with_pr_list_all(Ok(vec![pr_all_summary(
                99,
                "jowi-dev/proj-3-done-thing",
                crate::github::gh_cli::PrLifecycle::Merged,
            )]));
        let bots = cursor_bot();
        let mut out = Vec::new();

        let err =
            check(&ready_ctx(&jira, &gh, &bots), "proj-1", &mut out).expect_err("should fail");

        let rendered = err.to_string();
        assert!(rendered.contains("PROJ-1 is blocked by:"));
        assert!(rendered.contains("PROJ-2 (In Progress): Summary for PROJ-2"));
        assert!(
            !rendered.contains("PROJ-3"),
            "the merged-PR blocker should not be listed: {rendered}"
        );
        assert!(out.is_empty(), "nothing should be printed on failure");
        assert!(
            !String::from_utf8(out.clone()).unwrap().contains("warning"),
            "the blocked path must never perform a bot-findings lookup"
        );
    }

    #[test]
    fn check_blocked_ticket_is_an_error_listing_unmerged_blockers_and_excluding_a_done_one() {
        // PROJ-3 is Done in Jira with no discoverable PR at all (a config
        // change, a spike, docs, manual ops work — plenty of real tickets
        // never have a PR), so it's satisfied without ever consulting `gh`
        // for it specifically. PROJ-2 has no PR either, so it's the sole
        // unmerged blocker and the ticket is `BlockedNoPr`, not
        // `BlockedMultiple`.
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
                inward_issue: Some(linked_issue("PROJ-3", "Done", "done")),
                outward_issue: None,
            },
        ];
        let jira = FakeJiraClient::new().with_issue("PROJ-1", i);
        let gh = FakeGhCli::new().with_pr_list_all(Ok(vec![]));
        let bots = cursor_bot();
        let mut out = Vec::new();

        let err =
            check(&ready_ctx(&jira, &gh, &bots), "proj-1", &mut out).expect_err("should fail");

        let rendered = err.to_string();
        assert!(rendered.contains("PROJ-1 is blocked by:"));
        assert!(rendered.contains("PROJ-2 (In Progress): Summary for PROJ-2"));
        assert!(
            !rendered.contains("PROJ-3"),
            "the Jira-Done blocker should not be listed: {rendered}"
        );
        assert!(out.is_empty(), "nothing should be printed on failure");
    }

    #[test]
    fn check_invalid_key_is_an_actionable_error() {
        let jira = FakeJiraClient::new();
        let gh = FakeGhCli::new();
        let bots = cursor_bot();
        let mut out = Vec::new();

        let err =
            check(&ready_ctx(&jira, &gh, &bots), "not-a-key!", &mut out).expect_err("should fail");

        match err {
            ReadyCliError::InvalidKey { key } => assert_eq!(key, "not-a-key!"),
            other => panic!("expected InvalidKey, got {other:?}"),
        }
    }

    #[test]
    fn check_not_found_error_passes_through() {
        let jira = FakeJiraClient::new().with_issue_not_found("PROJ-404");
        let gh = FakeGhCli::new();
        let bots = cursor_bot();
        let mut out = Vec::new();

        let err =
            check(&ready_ctx(&jira, &gh, &bots), "proj-404", &mut out).expect_err("should fail");

        match err {
            ReadyCliError::Ticketing(TicketingError::Provider(
                crate::ticketing::error::ProviderError::NotFound { key },
            )) => {
                assert_eq!(key, "PROJ-404")
            }
            other => panic!("expected Jira NotFound, got {other:?}"),
        }
    }

    // --- The four-way stack decision matrix, via `check` ---
    //
    // Ready / stackable / blocked-multiple / blocked-no-PR, mirroring
    // `crate::blocker_stacking`'s decision table exactly (see
    // `check_ready_and_resolve_blocker_stacking_agree_on_the_same_stackable_ticket`
    // below for the test that guards the two call sites can't drift apart).

    #[test]
    fn check_one_unmerged_blocker_with_open_pr_is_stackable() {
        let mut ticket = issue("AX-409");
        ticket.fields.issue_links = vec![IssueLink {
            id: "1".to_string(),
            link_type: blocks_link_type(),
            inward_issue: Some(linked_issue("AX-408", "Code Review", "indeterminate")),
            outward_issue: None,
        }];
        let jira = FakeJiraClient::new().with_issue("AX-409", ticket);
        let gh = FakeGhCli::new().with_pr_list_all(Ok(vec![pr_all_summary(
            490,
            "jowi-dev/ax-408-20260810-131418",
            crate::github::gh_cli::PrLifecycle::Open,
        )]));
        let bots = cursor_bot();
        let mut out = Vec::new();

        let outcome = check(&ready_ctx(&jira, &gh, &bots), "ax-409", &mut out)
            .expect("stackable is not an error");

        assert_eq!(outcome, ReadyOutcome::Stackable);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "AX-409 is stackable on origin/jowi-dev/ax-408-20260810-131418 \
             (blocked by AX-408, PR #490 open)\n"
        );
    }

    #[test]
    fn check_two_unmerged_blockers_is_blocked_and_explains_why() {
        let mut ticket = issue("AX-409");
        ticket.fields.issue_links = vec![
            IssueLink {
                id: "1".to_string(),
                link_type: blocks_link_type(),
                inward_issue: Some(linked_issue("AX-408", "Code Review", "indeterminate")),
                outward_issue: None,
            },
            IssueLink {
                id: "2".to_string(),
                link_type: blocks_link_type(),
                inward_issue: Some(linked_issue("AX-410", "In Progress", "indeterminate")),
                outward_issue: None,
            },
        ];
        let jira = FakeJiraClient::new().with_issue("AX-409", ticket);
        let gh = FakeGhCli::new().with_pr_list_all(Ok(vec![pr_all_summary(
            490,
            "jowi-dev/ax-408-20260810-131418",
            crate::github::gh_cli::PrLifecycle::Open,
        )]));
        let bots = cursor_bot();
        let mut out = Vec::new();

        let err = check(&ready_ctx(&jira, &gh, &bots), "ax-409", &mut out)
            .expect_err("two unmerged blockers must not be stackable");

        let rendered = err.to_string();
        assert!(rendered.contains("AX-408"));
        assert!(rendered.contains("AX-410"));
        assert!(
            rendered.contains("can't stack on more than one"),
            "should explain why two unmerged blockers can't both be stacked on: {rendered}"
        );
        assert!(out.is_empty(), "nothing should be printed on failure");
    }

    #[test]
    fn check_one_unmerged_blocker_with_no_pr_is_blocked() {
        let mut ticket = issue("AX-409");
        ticket.fields.issue_links = vec![IssueLink {
            id: "1".to_string(),
            link_type: blocks_link_type(),
            inward_issue: Some(linked_issue("AX-408", "In Progress", "indeterminate")),
            outward_issue: None,
        }];
        let jira = FakeJiraClient::new().with_issue("AX-409", ticket);
        let gh = FakeGhCli::new().with_pr_list_all(Ok(vec![]));
        let bots = cursor_bot();
        let mut out = Vec::new();

        let err = check(&ready_ctx(&jira, &gh, &bots), "ax-409", &mut out)
            .expect_err("no PR to stack on yet must not be ready or stackable");

        let rendered = err.to_string();
        assert!(rendered.contains("AX-408"));
        assert!(rendered.contains("no PR found yet to stack on"));
        assert!(out.is_empty(), "nothing should be printed on failure");
    }

    #[test]
    fn check_transient_gh_failure_falls_back_to_jira_only_readiness() {
        let mut ticket = issue("AX-409");
        ticket.fields.issue_links = vec![IssueLink {
            id: "1".to_string(),
            link_type: blocks_link_type(),
            inward_issue: Some(linked_issue("AX-408", "Done", "done")),
            outward_issue: None,
        }];
        let jira = FakeJiraClient::new().with_issue("AX-409", ticket);
        let gh = FakeGhCli::new().with_pr_list_all(Err(GhError::Command {
            command: "gh pr list".to_string(),
            exit_code: Some(1),
            stderr: "boom".to_string(),
        }));
        let bots = cursor_bot();
        let mut out = Vec::new();

        // AX-408 is Jira-Done, so the Jira-only fallback (today's
        // pre-stacking behavior) reports the ticket ready despite the
        // unresolved gh lookup — a network hiccup must not freeze a ticket
        // Jira itself has no problem with.
        let outcome = check(&ready_ctx(&jira, &gh, &bots), "ax-409", &mut out)
            .expect("a transient gh failure must degrade, not error");

        assert_eq!(outcome, ReadyOutcome::Ready);
        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("warning: could not resolve blocker PRs"));
        assert!(output.contains("falling back to Jira-only readiness check"));
        assert!(output.contains("AX-409 is ready"));
    }

    #[test]
    fn check_permanent_gh_failure_is_a_loud_bug_not_a_quiet_fallback() {
        let mut ticket = issue("AX-409");
        ticket.fields.issue_links = vec![IssueLink {
            id: "1".to_string(),
            link_type: blocks_link_type(),
            inward_issue: Some(linked_issue("AX-408", "In Progress", "indeterminate")),
            outward_issue: None,
        }];
        let jira = FakeJiraClient::new().with_issue("AX-409", ticket);
        let gh = FakeGhCli::new().with_pr_list_all(Err(GhError::Command {
            command: "gh pr list".to_string(),
            exit_code: Some(1),
            stderr: r#"unknown JSON field: "merged""#.to_string(),
        }));
        let bots = cursor_bot();
        let mut out = Vec::new();

        let err = check(&ready_ctx(&jira, &gh, &bots), "ax-409", &mut out)
            .expect_err("a permanent gh failure must not be silently swallowed");

        match err {
            ReadyCliError::GhBug { key, .. } => assert_eq!(key, "AX-409"),
            other => panic!("expected GhBug, got {other:?}"),
        }
        assert!(out.is_empty(), "nothing should be printed for a hard error");
    }

    // --- The drift-proof test ---

    #[test]
    fn check_ready_and_resolve_blocker_stacking_agree_on_the_same_stackable_ticket() {
        // The whole point of `crate::blocker_stacking` is that `tm ready`
        // (report) and `resolve_blocker_stacking` (act, in `tm work run`)
        // can never independently decide differently for the same ticket —
        // this is the regression test for the incident that motivated the
        // split (AX-409/AX-436 vs AX-408's open PR — see that module's doc
        // comment). Same Jira issue, same PR list, fed to both call sites.
        let mut ticket = issue("AX-409");
        ticket.fields.issue_links = vec![IssueLink {
            id: "1".to_string(),
            link_type: blocks_link_type(),
            inward_issue: Some(linked_issue("AX-408", "Code Review", "indeterminate")),
            outward_issue: None,
        }];
        let jira = FakeJiraClient::new().with_issue("AX-409", ticket);
        let gh = FakeGhCli::new().with_pr_list_all(Ok(vec![pr_all_summary(
            490,
            "jowi-dev/ax-408-20260810-131418",
            crate::github::gh_cli::PrLifecycle::Open,
        )]));
        let bots = cursor_bot();
        let mut out = Vec::new();

        let ready_outcome = check(&ready_ctx(&jira, &gh, &bots), "ax-409", &mut out)
            .expect("tm ready should report this ticket stackable, not blocked");
        assert_eq!(ready_outcome, ReadyOutcome::Stackable);
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("jowi-dev/ax-408-20260810-131418"),
            "tm ready should name the stack base"
        );

        let resolution = crate::work::run::resolve_blocker_stacking(
            Some(&jira),
            &gh,
            std::path::Path::new("/repo"),
            Some("AX-409"),
            &crate::blocker_stacking::InFlight::new(),
            &|_| false,
        )
        .expect("resolve_blocker_stacking should also treat this as stackable, not refuse");
        assert_eq!(
            resolution.stacked_base,
            Some("origin/jowi-dev/ax-408-20260810-131418".to_string()),
            "tm work run must stack on exactly the branch tm ready named"
        );
    }

    #[test]
    fn check_ready_and_resolve_blocker_stacking_agree_a_done_blocker_with_no_pr_is_ready() {
        // Same drift-proof shape as the stackable case above, but for the
        // other half of the satisfaction rule: a blocker that's Done in
        // Jira with no PR at all must read as `Ready` at both call sites,
        // not just at `tm ready`'s Jira-only fallback path.
        let mut ticket = issue("AX-409");
        ticket.fields.issue_links = vec![IssueLink {
            id: "1".to_string(),
            link_type: blocks_link_type(),
            inward_issue: Some(linked_issue("AX-408", "Done", "done")),
            outward_issue: None,
        }];
        let jira = FakeJiraClient::new().with_issue("AX-409", ticket);
        let gh = FakeGhCli::new().with_pr_list_all(Ok(vec![]));
        let bots = cursor_bot();
        let mut out = Vec::new();

        let ready_outcome = check(&ready_ctx(&jira, &gh, &bots), "ax-409", &mut out)
            .expect("tm ready should report this ticket ready, not blocked");
        assert_eq!(ready_outcome, ReadyOutcome::Ready);

        let resolution = crate::work::run::resolve_blocker_stacking(
            Some(&jira),
            &gh,
            std::path::Path::new("/repo"),
            Some("AX-409"),
            &crate::blocker_stacking::InFlight::new(),
            &|_| false,
        )
        .expect("resolve_blocker_stacking should also treat this as ready, not refuse");
        assert_eq!(
            resolution.stacked_base, None,
            "a Done blocker needs no stack base override"
        );
    }

    // --- `list`'s stackable surfacing ---

    #[test]
    fn list_surfaces_a_stackable_ticket_instead_of_hiding_it() {
        let mut stackable = issue("AX-409");
        stackable.fields.issue_links = vec![IssueLink {
            id: "1".to_string(),
            link_type: blocks_link_type(),
            inward_issue: Some(linked_issue("AX-408", "Code Review", "indeterminate")),
            outward_issue: None,
        }];
        let jira =
            FakeJiraClient::new().with_search_result(search_result(vec![issue("AX-1"), stackable]));
        let gh = FakeGhCli::new().with_pr_list_all(Ok(vec![pr_all_summary(
            490,
            "jowi-dev/ax-408-20260810-131418",
            crate::github::gh_cli::PrLifecycle::Open,
        )]));
        let bots = cursor_bot();
        let mut out = Vec::new();

        list(&ready_ctx(&jira, &gh, &bots), &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("AX-1  Summary for AX-1\n"));
        assert!(output.contains(
            "AX-409  Summary for AX-409  [stackable on origin/jowi-dev/ax-408-20260810-131418 \
             — blocked by AX-408, PR #490 open]\n"
        ));
        assert!(
            !output.contains("hidden"),
            "a stackable ticket must not also count as hidden: {output}"
        );
    }

    #[test]
    fn list_stackability_lookup_failure_degrades_to_the_tracker_only_answer() {
        let jira = FakeJiraClient::new().with_search_result(search_result(vec![
            issue("AX-1"),
            blocked_issue("AX-409", &[("AX-408", "Code Review", "indeterminate")]),
            blocked_issue("AX-410", &[("AX-408", "Done", "done")]),
        ]));
        let gh = FakeGhCli::new().with_pr_list_all(Err(GhError::Command {
            command: "gh pr list".to_string(),
            exit_code: Some(1),
            stderr: "boom".to_string(),
        }));
        let bots = cursor_bot();
        let mut out = Vec::new();

        list(&ready_ctx(&jira, &gh, &bots), &mut out).expect("should still succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(output.starts_with("warning: could not check stackability"));
        assert!(output.contains("AX-1  Summary for AX-1\n"));
        assert!(output.contains("AX-410  Summary for AX-410\n"));
        assert!(output.contains(
            "AX-409  Summary for AX-409  [blocked by AX-408 (Code Review) — blocker PRs unresolved]\n"
        ));
        assert!(!output.contains("stackable on"));
    }

    // --- List/check parity ---

    /// What `check` concluded, collapsed to the `decision` tag `list_json`
    /// emits, so the two forms can be compared directly.
    fn check_decision(ctx: &ReadyContext, key: &str) -> &'static str {
        match check(ctx, key, &mut Vec::new()) {
            Ok(ReadyOutcome::Ready) => "ready",
            Ok(ReadyOutcome::Stackable) => "stackable",
            Err(ReadyCliError::NotReady { .. }) => "blocked",
            Err(other) => panic!("unexpected check error for {key}: {other}"),
        }
    }

    #[test]
    fn list_and_check_agree_on_every_decision_table_row() {
        use crate::github::gh_cli::PrLifecycle;
        let fixtures = vec![
            issue("AX-1"),
            blocked_issue("AX-2", &[("AX-90", "Done", "done")]),
            blocked_issue("AX-3", &[("AX-91", "In Review", "indeterminate")]),
            blocked_issue("AX-4", &[("AX-92", "Code Review", "indeterminate")]),
            blocked_issue("AX-5", &[("AX-93", "To Do", "new")]),
            blocked_issue(
                "AX-6",
                &[
                    ("AX-92", "Code Review", "indeterminate"),
                    ("AX-93", "To Do", "new"),
                ],
            ),
            blocked_issue("AX-7", &[("AX-94", "In Progress", "indeterminate")]),
        ];
        let prs = vec![
            pr_all_summary(91, "jowi-dev/ax-91-merged", PrLifecycle::Merged),
            pr_all_summary(92, "jowi-dev/ax-92-open", PrLifecycle::Open),
            pr_all_summary(94, "jowi-dev/ax-94-closed", PrLifecycle::Closed),
        ];
        let mut jira = FakeJiraClient::new().with_search_result(search_result(fixtures.clone()));
        for fixture in &fixtures {
            jira = jira.with_issue(&fixture.key, fixture.clone());
        }
        let gh = FakeGhCli::new().with_pr_list_all(Ok(prs));
        let bots = cursor_bot();
        let ctx = ready_ctx(&jira, &gh, &bots);
        let mut out = Vec::new();

        list_json(&ctx, &mut out).expect("should succeed");

        let value: serde_json::Value = serde_json::from_slice(&out).expect("valid JSON");
        let listed: Vec<(String, String)> = value["tickets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| {
                (
                    t["key"].as_str().unwrap().to_string(),
                    t["decision"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        let checked: Vec<(String, String)> = fixtures
            .iter()
            .map(|f| (f.key.clone(), check_decision(&ctx, &f.key).to_string()))
            .collect();
        assert_eq!(listed, checked);
        assert_eq!(
            checked.iter().map(|(_, d)| d.as_str()).collect::<Vec<_>>(),
            vec![
                "ready",
                "ready",
                "ready",
                "stackable",
                "blocked",
                "blocked",
                "blocked"
            ],
            "fixtures should cover every decision"
        );
    }

    // --- Audit gate (GitHub issue #112) ---

    /// A temp-dir run store holding each `(key, verdict)` audit, in order.
    fn audit_store(audits: &[(&str, &str)]) -> (tempfile::TempDir, crate::runs::RunStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();
        for (key, verdict) in audits {
            store.record_audit("scope", key, verdict, None).unwrap();
        }
        (dir, store)
    }

    fn gated<'a>(ctx: ReadyContext<'a>, store: &'a crate::runs::RunStore) -> ReadyContext<'a> {
        ReadyContext {
            audit: AuditGate::Store {
                store,
                scope: "scope".to_string(),
            },
            ..ctx
        }
    }

    #[test]
    fn list_shows_unaudited_and_failed_audit_tickets_as_needing_audit() {
        let jira = FakeJiraClient::new().with_search_result(search_result(vec![
            issue("PROJ-1"),
            issue("PROJ-2"),
            issue("PROJ-3"),
        ]));
        let gh = FakeGhCli::new();
        let bots = cursor_bot();
        let (_dir, store) = audit_store(&[("PROJ-1", "ready"), ("PROJ-3", "needs-work")]);
        let mut out = Vec::new();

        list(&gated(ready_ctx(&jira, &gh, &bots), &store), &mut out).expect("should succeed");

        assert_eq!(
            String::from_utf8(out).unwrap(),
            "PROJ-1  Summary for PROJ-1\n\
             PROJ-2  Summary for PROJ-2  [needs audit: not audited]\n\
             PROJ-3  Summary for PROJ-3  [needs audit: last audit was needs-work]\n"
        );
    }

    #[test]
    fn list_with_only_unaudited_tickets_prints_no_ready_tickets() {
        let jira = FakeJiraClient::new().with_search_result(search_result(vec![issue("PROJ-1")]));
        let gh = FakeGhCli::new();
        let bots = cursor_bot();
        let (_dir, store) = audit_store(&[]);
        let mut out = Vec::new();

        list(&gated(ready_ctx(&jira, &gh, &bots), &store), &mut out).expect("should succeed");

        assert_eq!(
            String::from_utf8(out).unwrap(),
            "No ready tickets.\nPROJ-1  Summary for PROJ-1  [needs audit: not audited]\n"
        );
    }

    #[test]
    fn a_later_failing_audit_removes_a_ticket_from_ready() {
        let jira = FakeJiraClient::new()
            .with_search_result(search_result(vec![issue("PROJ-1")]))
            .with_issue("PROJ-1", issue("PROJ-1"));
        let gh = FakeGhCli::new();
        let bots = cursor_bot();
        let (_dir, store) = audit_store(&[("PROJ-1", "ready")]);
        // Same-millisecond rows tie on `audited_at`; `id` breaks the tie.
        store
            .record_audit("scope", "PROJ-1", "needs-work", None)
            .unwrap();
        let ctx = gated(ready_ctx(&jira, &gh, &bots), &store);
        let mut out = Vec::new();

        list_json(&ctx, &mut out).expect("should succeed");
        let err = check(&ctx, "PROJ-1", &mut Vec::new()).expect_err("failed audit");

        let value: serde_json::Value = serde_json::from_slice(&out).expect("valid JSON");
        assert_eq!(
            value["tickets"][0],
            serde_json::json!({
                "key": "PROJ-1",
                "summary": "Summary for PROJ-1",
                "decision": "needs_audit",
                "reason": "last audit was needs-work",
                "unblocks": 0,
            })
        );
        assert!(matches!(err, ReadyCliError::NotAudited { .. }), "{err:?}");
    }

    #[test]
    fn list_keeps_a_blocked_tickets_blocker_reason_over_its_audit() {
        let jira = FakeJiraClient::new().with_search_result(search_result(vec![blocked_issue(
            "PROJ-2",
            &[("PROJ-9", "In Progress", "indeterminate")],
        )]));
        let gh = FakeGhCli::new();
        let bots = cursor_bot();
        let (_dir, store) = audit_store(&[]);
        let mut out = Vec::new();

        list(&gated(ready_ctx(&jira, &gh, &bots), &store), &mut out).expect("should succeed");

        assert_eq!(
            String::from_utf8(out).unwrap(),
            "No ready tickets.\n\
             PROJ-2  Summary for PROJ-2  [blocked by PROJ-9 (In Progress), no PR yet]\n"
        );
    }

    #[test]
    fn list_treats_every_ticket_as_needing_audit_when_the_store_is_unavailable() {
        let jira = FakeJiraClient::new().with_search_result(search_result(vec![issue("PROJ-1")]));
        let gh = FakeGhCli::new();
        let bots = cursor_bot();
        let ctx = ReadyContext {
            audit: AuditGate::Unavailable("disk on fire".to_string()),
            ..ready_ctx(&jira, &gh, &bots)
        };
        let mut out = Vec::new();

        list(&ctx, &mut out).expect("should succeed");

        assert_eq!(
            String::from_utf8(out).unwrap(),
            "No ready tickets.\n\
             PROJ-1  Summary for PROJ-1  [needs audit: audit store unavailable: disk on fire]\n"
        );
    }

    #[test]
    fn check_reports_an_unaudited_ticket_as_not_audited() {
        let jira = FakeJiraClient::new().with_issue("PROJ-1", issue("PROJ-1"));
        let gh = FakeGhCli::new();
        let bots = cursor_bot();
        let (_dir, store) = audit_store(&[]);
        let mut out = Vec::new();

        let err = check(
            &gated(ready_ctx(&jira, &gh, &bots), &store),
            "proj-1",
            &mut out,
        )
        .expect_err("unaudited is not ready");

        assert_eq!(
            err.to_string(),
            "PROJ-1 needs audit (not audited); record one with `tm ticket audit PROJ-1 --record ready`"
        );
        assert!(out.is_empty(), "nothing printed as ready: {out:?}");
    }

    #[test]
    fn check_passes_a_ticket_whose_latest_audit_is_ready() {
        let jira = FakeJiraClient::new().with_issue("PROJ-1", issue("PROJ-1"));
        let gh = FakeGhCli::new();
        let bots = cursor_bot();
        let (_dir, store) = audit_store(&[("PROJ-1", "needs-work"), ("PROJ-1", "ready")]);
        let mut out = Vec::new();

        let outcome = check(
            &gated(ready_ctx(&jira, &gh, &bots), &store),
            "PROJ-1",
            &mut out,
        )
        .expect("audited ticket is ready");

        assert_eq!(outcome, ReadyOutcome::Ready);
        assert_eq!(String::from_utf8(out).unwrap(), "PROJ-1 is ready (To Do)\n");
    }

    #[test]
    fn check_gates_a_stackable_ticket_on_its_audit_too() {
        let ticket = blocked_issue("AX-409", &[("AX-408", "Code Review", "indeterminate")]);
        let jira = FakeJiraClient::new().with_issue("AX-409", ticket);
        let gh = FakeGhCli::new().with_pr_list_all(Ok(vec![pr_all_summary(
            490,
            "jowi-dev/ax-408-thing",
            crate::github::gh_cli::PrLifecycle::Open,
        )]));
        let bots = cursor_bot();
        let (_dir, store) = audit_store(&[]);

        let err = check(
            &gated(ready_ctx(&jira, &gh, &bots), &store),
            "AX-409",
            &mut Vec::new(),
        )
        .expect_err("unaudited stackable is not workable");

        assert!(matches!(err, ReadyCliError::NotAudited { .. }), "{err:?}");
    }

    #[test]
    fn check_keeps_a_blocked_ticket_blocked_whatever_its_audit() {
        let ticket = blocked_issue("AX-409", &[("AX-408", "In Progress", "indeterminate")]);
        let jira = FakeJiraClient::new().with_issue("AX-409", ticket);
        let gh = FakeGhCli::new().with_pr_list_all(Ok(vec![]));
        let bots = cursor_bot();
        let (_dir, store) = audit_store(&[]);

        let err = check(
            &gated(ready_ctx(&jira, &gh, &bots), &store),
            "AX-409",
            &mut Vec::new(),
        )
        .expect_err("blocked");

        assert!(matches!(err, ReadyCliError::NotReady { .. }), "{err:?}");
    }
}
