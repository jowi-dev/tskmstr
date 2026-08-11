//! `tm ready` and `tm ready <KEY>`.
//!
//! `tm ready` (no key) lists tickets assigned to the current user that are
//! ready to pick up: see [`crate::ticketing::ready_tickets`] for the exact
//! candidate query and Jira-status blocker filter. `tm ready <KEY>` checks
//! one specific ticket (any assignee, any status), classifying it as
//! ready, **stackable**, or blocked via [`crate::blocker_stacking::decide`]
//! — the same decision table [`crate::work::run::resolve_blocker_stacking`]
//! uses to actually cut a stacked branch, so the two can't disagree. Ready
//! and stackable are both `Ok(`[`ReadyOutcome`]`)`, distinguished so `main.rs`
//! can give each its own exit code (see [`ReadyOutcome`]'s doc comment);
//! blocked is [`ReadyCliError::NotReady`], turned into a non-zero exit by
//! `main.rs`'s existing error path without special-casing `tm ready` for
//! that case.
//!
//! Both forms also carry a best-effort, ADVISORY annotation of GitHub bot
//! review findings (see [`crate::github::bot_findings`]) on a ready ticket's
//! associated pull request, if any. This is purely informational: it never
//! hides a ticket, changes an exit code, or turns a `gh` failure into an
//! error, since a bot false positive must not freeze a ticket. See
//! [`bot_finding_annotations`] and [`print_bot_finding_note`] for the lookup.

use std::collections::HashMap;
use std::io::Write;

use thiserror::Error;

use crate::blocker_stacking::{self, StackDecision};
use crate::github::bot_findings::count_bot_findings;
use crate::github::gh_cli::{GhCli, GhError};
use crate::github::pr::{PrInfo, find_issue_key};
use crate::jira::client::JiraClient;
use crate::jira::types::Issue;
use crate::ticketing::{TicketingError, open_blockers, ready_tickets};

/// Dependencies `tm ready`'s bot-findings annotation needs, alongside the
/// Jira client, bundled to keep `list`/`check`'s arity within this project's
/// convention (see `TicketingContext`).
pub struct ReadyContext<'a> {
    /// Jira client used for the readiness query/check itself.
    pub jira: &'a dyn JiraClient,
    /// `gh` client used for the best-effort bot-findings annotation.
    pub gh: &'a dyn GhCli,
    /// Bot logins configured as [`crate::config::Config::review_bots`].
    pub review_bots: &'a [String],
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

/// `tm ready` (no key): print tickets assigned to the current user that are
/// ready to pick up, one per line as `KEY  Summary`, in rank order, followed
/// by any candidate that's blocked but stackable (see
/// [`crate::blocker_stacking`]), as `KEY  Summary  [stackable on <branch> —
/// blocked by <BLOCKER>, PR #<N> open]`.
///
/// Prints `No ready tickets.` if none are ready or stackable. If any
/// remaining candidates were excluded for having an unmerged blocker with
/// nothing to stack on (or more than one unmerged blocker), appends a final
/// `(N blocked tickets hidden)` line so a filtered list doesn't read as
/// "this is everything assigned to you". Always exits 0.
///
/// Each ready line also carries a best-effort, advisory bot-findings
/// annotation: when a ready ticket has an open PR (matched by title) with
/// unresolved bot review findings, the line becomes `KEY  Summary  [N
/// unresolved bot findings]` (singular "finding" when `N == 1`). Zero
/// unresolved or no matching PR leaves the line unchanged. See
/// [`bot_finding_annotations`] for how that lookup degrades if `gh` fails;
/// see [`stackable_and_hidden`] for how the separate stackability lookup
/// degrades (every blocked ticket stays hidden, same as before this state
/// existed).
pub fn list(ctx: &ReadyContext, out: &mut dyn Write) -> Result<(), ReadyCliError> {
    let listing = ready_tickets(ctx.jira)?;
    let (stackable, hidden_count) = stackable_and_hidden(ctx, &listing.blocked, out)?;

    if listing.ready.is_empty() && stackable.is_empty() {
        writeln!(out, "No ready tickets.")?;
    } else {
        let annotations = bot_finding_annotations(ctx, &listing.ready, out)?;
        for issue in &listing.ready {
            match annotations.get(&issue.key) {
                Some(unresolved) if *unresolved > 0 => {
                    let noun = if *unresolved == 1 {
                        "finding"
                    } else {
                        "findings"
                    };
                    writeln!(
                        out,
                        "{}  {}  [{unresolved} unresolved bot {noun}]",
                        issue.key, issue.fields.summary
                    )?;
                }
                _ => writeln!(out, "{}  {}", issue.key, issue.fields.summary)?,
            }
        }
        for ticket in &stackable {
            writeln!(
                out,
                "{}  {}  [stackable on {} — blocked by {}, PR #{} open]",
                ticket.issue.key,
                ticket.issue.fields.summary,
                ticket.base,
                ticket.blocker_key,
                ticket.pr_number
            )?;
        }
    }

    if hidden_count > 0 {
        let noun = if hidden_count == 1 {
            "ticket"
        } else {
            "tickets"
        };
        writeln!(out, "({hidden_count} blocked {noun} hidden)")?;
    }

    Ok(())
}

/// A [`list`] candidate found stackable by [`stackable_and_hidden`].
struct StackableTicket {
    issue: Issue,
    base: String,
    blocker_key: String,
    pr_number: u64,
}

/// For [`list`]: split `blocked` (candidates [`ready_tickets`] excluded for
/// having an open Jira blocker) into tickets that are stackable — an open PR
/// to build on, per [`crate::blocker_stacking::decide`] — versus tickets
/// that remain genuinely hidden (no PR yet, or two or more unmerged
/// blockers). This is the same rule [`check`] and
/// [`crate::work::run::resolve_blocker_stacking`] use, so a ticket `list`
/// reports stackable is never one `tm ready <KEY>` would separately call
/// blocked.
///
/// Calls [`GhCli::pr_list_all`] once for every blocked candidate, same
/// one-call-per-listing shape as [`bot_finding_annotations`]. On failure,
/// prints `warning: could not check stackability: {err}` (permanent errors
/// flagged via [`crate::github::gh_cli::permanence_note`]) and treats every
/// blocked candidate as hidden — today's pre-stacking behavior — since a
/// `gh` hiccup must never mislabel a genuinely-blocked ticket as pickable,
/// and a whole-listing hard failure would be disproportionate for a
/// best-effort annotation.
fn stackable_and_hidden(
    ctx: &ReadyContext,
    blocked: &[Issue],
    out: &mut dyn Write,
) -> Result<(Vec<StackableTicket>, usize), ReadyCliError> {
    if blocked.is_empty() {
        return Ok((Vec::new(), 0));
    }

    let cwd = std::env::current_dir()?;
    let prs = match ctx.gh.pr_list_all(&cwd) {
        Ok(prs) => prs,
        Err(err) => {
            let note = crate::github::gh_cli::permanence_note(&err);
            writeln!(out, "warning: could not check stackability: {err}{note}")?;
            return Ok((Vec::new(), blocked.len()));
        }
    };

    let mut stackable = Vec::new();
    let mut hidden_count = 0;
    for issue in blocked {
        let unmerged = blocker_stacking::unmerged_direct_blockers(issue, &prs);
        match blocker_stacking::decide(unmerged) {
            StackDecision::Stackable {
                blocker_key,
                pr_number,
                head_ref_name,
            } => stackable.push(StackableTicket {
                issue: issue.clone(),
                base: format!("origin/{head_ref_name}"),
                blocker_key,
                pr_number,
            }),
            StackDecision::Ready
            | StackDecision::BlockedNoPr { .. }
            | StackDecision::BlockedMultiple { .. } => {
                hidden_count += 1;
            }
        }
    }

    Ok((stackable, hidden_count))
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
/// - Exactly one, with no PR yet, or two or more → returns
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
        .map_err(TicketingError::Jira)?;
    let status_name = issue.fields.status.name.clone();

    if blocker_stacking::direct_blockers(&issue).is_empty() {
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

    let unmerged = blocker_stacking::unmerged_direct_blockers(&issue, &prs);
    match blocker_stacking::decide(unmerged) {
        StackDecision::Ready => {
            writeln!(out, "{normalized} is ready ({status_name})")?;
            print_bot_finding_note(ctx, &normalized, out)?;
            Ok(ReadyOutcome::Ready)
        }
        StackDecision::Stackable {
            blocker_key,
            pr_number,
            head_ref_name,
        } => {
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

/// For `list`: compute unresolved bot-finding counts for every ticket in
/// `tickets` that has a matching open PR, degrading to a printed warning
/// (once) rather than an error if `gh` fails.
///
/// Calls `gh.pr_list()` once. If it fails, prints
/// `warning: could not check bot findings: {err}` and returns an empty map —
/// the listing is then printed unannotated. Otherwise, for each ticket
/// matched to an open PR (by [`find_issue_key`] against the PR title), calls
/// `gh.pr_review_threads`; a failure there prints the same warning line
/// exactly once (not once per ticket) and simply omits that ticket (and any
/// ticket whose lookup fails afterward) from the returned map, without
/// aborting the remaining lookups.
fn bot_finding_annotations(
    ctx: &ReadyContext,
    tickets: &[Issue],
    out: &mut dyn Write,
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
            writeln!(out, "warning: could not check bot findings: {err}{note}")?;
            return Ok(annotations);
        }
    };

    let mut warned = false;
    for issue in tickets {
        let Some(number) = matching_pr_number(&prs, &issue.key) else {
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
                    writeln!(out, "warning: could not check bot findings: {err}{note}")?;
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

    let Some(number) = matching_pr_number(&prs, key) else {
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
/// reusing [`find_issue_key`] — the same title/body/branch extraction `tm pr
/// status` uses — rather than re-implementing key parsing.
fn matching_pr_number(prs: &[PrInfo], key: &str) -> Option<u64> {
    prs.iter()
        .find(|pr| find_issue_key(pr).as_deref() == Some(key))
        .map(|pr| pr.number)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::bot_findings::ReviewThread;
    use crate::github::gh_cli::{FakeGhCli, GhError};
    use crate::jira::fake::FakeJiraClient;
    use crate::jira::types::{
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

    #[test]
    fn list_appends_hidden_count_when_candidates_are_blocked() {
        let mut blocked = issue("PROJ-2");
        blocked.fields.issue_links = vec![IssueLink {
            id: "10001".to_string(),
            link_type: blocks_link_type(),
            inward_issue: Some(linked_issue("PROJ-9", "In Progress", "indeterminate")),
            outward_issue: None,
        }];
        let jira =
            FakeJiraClient::new().with_search_result(search_result(vec![issue("PROJ-1"), blocked]));
        let gh = FakeGhCli::new();
        let bots = cursor_bot();
        let mut out = Vec::new();

        list(&ready_ctx(&jira, &gh, &bots), &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(
            output,
            "PROJ-1  Summary for PROJ-1\n(1 blocked ticket hidden)\n"
        );
    }

    #[test]
    fn list_all_blocked_prints_no_ready_tickets_then_hidden_count() {
        let mut blocked = issue("PROJ-1");
        blocked.fields.issue_links = vec![IssueLink {
            id: "10001".to_string(),
            link_type: blocks_link_type(),
            inward_issue: Some(linked_issue("PROJ-9", "In Progress", "indeterminate")),
            outward_issue: None,
        }];
        let jira = FakeJiraClient::new().with_search_result(search_result(vec![blocked]));
        let gh = FakeGhCli::new();
        let bots = cursor_bot();
        let mut out = Vec::new();

        list(&ready_ctx(&jira, &gh, &bots), &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(output, "No ready tickets.\n(1 blocked ticket hidden)\n");
    }

    #[test]
    fn list_hidden_count_pluralizes_above_one() {
        let blocked = |key: &str| {
            let mut i = issue(key);
            i.fields.issue_links = vec![IssueLink {
                id: "10001".to_string(),
                link_type: blocks_link_type(),
                inward_issue: Some(linked_issue("PROJ-9", "In Progress", "indeterminate")),
                outward_issue: None,
            }];
            i
        };
        let jira = FakeJiraClient::new()
            .with_search_result(search_result(vec![blocked("PROJ-1"), blocked("PROJ-2")]));
        let gh = FakeGhCli::new();
        let bots = cursor_bot();
        let mut out = Vec::new();

        list(&ready_ctx(&jira, &gh, &bots), &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert_eq!(output, "No ready tickets.\n(2 blocked tickets hidden)\n");
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
            ReadyCliError::Ticketing(TicketingError::Jira(
                crate::jira::client::JiraError::NotFound { key },
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
    fn list_stackability_lookup_failure_degrades_to_hiding_the_blocked_ticket() {
        let mut blocked = issue("AX-409");
        blocked.fields.issue_links = vec![IssueLink {
            id: "1".to_string(),
            link_type: blocks_link_type(),
            inward_issue: Some(linked_issue("AX-408", "Code Review", "indeterminate")),
            outward_issue: None,
        }];
        let jira =
            FakeJiraClient::new().with_search_result(search_result(vec![issue("AX-1"), blocked]));
        let gh = FakeGhCli::new().with_pr_list_all(Err(GhError::Command {
            command: "gh pr list".to_string(),
            exit_code: Some(1),
            stderr: "boom".to_string(),
        }));
        let bots = cursor_bot();
        let mut out = Vec::new();

        list(&ready_ctx(&jira, &gh, &bots), &mut out).expect("should still succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("warning: could not check stackability"));
        assert!(output.contains("(1 blocked ticket hidden)"));
        assert!(!output.contains("stackable"));
    }
}
