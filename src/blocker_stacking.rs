//! The single decision table for "is this ticket's direct Jira blocker
//! actually still in the way, or can work stack on top of it instead of
//! waiting" — shared by [`crate::work::run::resolve_blocker_stacking`] (which
//! *acts* on the decision, cutting a run's branch from the stack base) and
//! `tm ready` (`crate::cli::ready`, which only *reports* it).
//!
//! # Why this exists
//!
//! Before this module, `tm ready` and `resolve_blocker_stacking` each
//! answered "can this ticket be worked on" with their own, disagreeing
//! logic: `tm ready` filtered blockers by Jira **status**
//! ([`crate::ticketing::open_blockers`]), while `resolve_blocker_stacking`
//! deliberately ignored Jira status and looked only at a blocker's **PR
//! merge state** instead. Real incident: AX-409 and AX-436 were both
//! blocked by AX-408, whose PR was open in Code Review. Stacking correctly
//! classified both as stackable, but `tm ready` still reported them as
//! `BLOCKED` (Jira status hadn't moved), and the autonomous lane prompt
//! ("don't touch tickets `tm ready` reports as blocked") took `tm ready` at
//! its word — six lane dispatches refused viable work, at real cost.
//!
//! `direct_blockers`, `unmerged_direct_blockers`, and [`decide`] are the one
//! place this decision is made. Both call sites feed it the same inputs (a
//! ticket's direct blockers, and the PR list for the blockers' repo) and get
//! the same [`StackDecision`] back — they cannot independently drift because
//! there is no second copy of the rule left to drift.
//!
//! # The rule
//!
//! For each of a ticket's *direct* `Blocks`-type Jira links (any status —
//! Jira status is never consulted, only whether a matching PR exists and
//! whether it's merged):
//! - PR **merged** → satisfied, not counted as unmerged.
//! - PR **open** → an unmerged blocker with something to stack on.
//! - **no PR** (including a closed-but-unmerged one) → an unmerged blocker
//!   with nothing to stack on yet.
//!
//! Then, across the ticket's unmerged blockers ([`decide`]):
//! - **zero** → [`StackDecision::Ready`].
//! - **exactly one, with an open PR** → [`StackDecision::Stackable`]: name
//!   the branch to stack on.
//! - **exactly one, with no PR** → [`StackDecision::BlockedNoPr`]: nothing to
//!   build on yet.
//! - **two or more** → [`StackDecision::BlockedMultiple`]: a single run
//!   branch can only be stacked on one dependency at a time, so this refuses
//!   rather than guessing which one.

use crate::github::gh_cli::{PrLifecycle, PrSummary};
use crate::jira::types::{Issue, LinkedIssue};

/// All direct `Blocks`-type blockers of `issue`, regardless of Jira status.
///
/// Unlike [`crate::ticketing::open_blockers`] (which drops blockers whose
/// Jira status is already `done`, since it's answering "is this ticket
/// pickable right now" from Jira's point of view alone), this module needs
/// every direct blocker no matter its Jira status: a blocker's *PR* merge
/// state — not its Jira status — is what decides whether it's "satisfied"
/// here, so filtering on Jira status would let a blocker with a stale
/// "Done" status but an unmerged PR slip through unstacked.
pub fn direct_blockers(issue: &Issue) -> Vec<&LinkedIssue> {
    issue
        .fields
        .issue_links
        .iter()
        .filter(|link| link.link_type.name == "Blocks")
        .filter_map(|link| link.inward_issue.as_ref())
        .collect()
}

/// Whether a PR's head branch (`head_ref_name`, e.g.
/// `jowi-dev/ax-410-add-connector`) belongs to the ticket keyed by
/// `key_lower`, per the lane-branch naming convention `<owner>/<ticket-key-
/// lowercased>-<suffix>` (`crate::work::naming::branch_name`/
/// `branch_name_with_slug`). `gh pr list` has no server-side "starts after
/// the first slash with" filter, so this is applied client-side.
fn head_branch_matches_ticket(head_ref_name: &str, key_lower: &str) -> bool {
    match head_ref_name.split_once('/') {
        Some((_, rest)) => rest.starts_with(&format!("{key_lower}-")),
        None => false,
    }
}

/// Find the PR (if any) for `blocker_key`'s lane branch among `prs` — every
/// entry whose head branch matches [`head_branch_matches_ticket`], preferring
/// an open one and, among ties, the most recently updated.
pub fn find_blocker_pr(prs: &[PrSummary], blocker_key: &str) -> Option<PrSummary> {
    let key_lower = blocker_key.to_lowercase();
    let mut matches: Vec<&PrSummary> = prs
        .iter()
        .filter(|pr| head_branch_matches_ticket(&pr.head_ref_name, &key_lower))
        .collect();
    matches.sort_by(|a, b| {
        let a_open = a.lifecycle == PrLifecycle::Open;
        let b_open = b.lifecycle == PrLifecycle::Open;
        b_open.cmp(&a_open).then(b.updated_at.cmp(&a.updated_at))
    });
    matches.into_iter().next().cloned()
}

/// One of a ticket's direct blockers that isn't (yet) satisfied by a merged
/// PR, carrying enough Jira context (`status_name`/`summary`) for a
/// human-readable report, not just the bare key `resolve_blocker_stacking`
/// needs to act.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnmergedBlocker {
    /// The blocker's issue key.
    pub key: String,
    /// The blocker's current Jira workflow status name, for display only —
    /// never consulted by [`decide`] itself.
    pub status_name: String,
    /// The blocker's one-line Jira summary, for display only.
    pub summary: String,
    /// `Some((number, head_ref_name))` when the blocker has an open PR to
    /// stack on; `None` when it has no PR (or only a closed, unmerged one —
    /// treated the same as "nothing to stack on").
    pub open_pr: Option<(u64, String)>,
}

/// Format `blocker` as `<KEY> (PR #N open)` or `<KEY> (no PR)`, the terse
/// form `resolve_blocker_stacking`'s error messages use.
pub fn format_unmerged_blocker(blocker: &UnmergedBlocker) -> String {
    match &blocker.open_pr {
        Some((number, _)) => format!("{} (PR #{number} open)", blocker.key),
        None => format!("{} (no PR)", blocker.key),
    }
}

/// Resolve `issue`'s direct blockers (via [`direct_blockers`]) against
/// already-fetched `prs`, keeping only those not satisfied by a merged PR.
pub fn unmerged_direct_blockers(issue: &Issue, prs: &[PrSummary]) -> Vec<UnmergedBlocker> {
    direct_blockers(issue)
        .into_iter()
        .filter_map(|blocker| match find_blocker_pr(prs, &blocker.key) {
            Some(pr) if pr.lifecycle == PrLifecycle::Merged => None,
            Some(pr) if pr.lifecycle == PrLifecycle::Open => Some(UnmergedBlocker {
                key: blocker.key.clone(),
                status_name: blocker.fields.status.name.clone(),
                summary: blocker.fields.summary.clone(),
                open_pr: Some((pr.number, pr.head_ref_name.clone())),
            }),
            _ => Some(UnmergedBlocker {
                key: blocker.key.clone(),
                status_name: blocker.fields.status.name.clone(),
                summary: blocker.fields.summary.clone(),
                open_pr: None,
            }),
        })
        .collect()
}

/// The decision [`decide`] returns: what a ticket's unmerged direct blockers
/// mean for whether work can proceed on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StackDecision {
    /// No unmerged direct blockers: nothing stopping this ticket.
    Ready,
    /// Exactly one unmerged direct blocker, with an open PR: work can stack
    /// on `origin/<head_ref_name>` instead of waiting for it to merge.
    Stackable {
        /// The blocker's issue key.
        blocker_key: String,
        /// The blocker's open PR number.
        pr_number: u64,
        /// The blocker's PR head branch — stack on `origin/<this>`.
        head_ref_name: String,
    },
    /// Exactly one unmerged direct blocker, but it has no PR yet: nothing
    /// exists to stack on.
    BlockedNoPr {
        /// The unsatisfied blocker.
        blocker: UnmergedBlocker,
    },
    /// Two or more unmerged direct blockers: a single branch can only be
    /// stacked on one dependency at a time, so this refuses rather than
    /// guessing which one.
    BlockedMultiple {
        /// Every unmerged blocker, in the order [`unmerged_direct_blockers`]
        /// found them.
        blockers: Vec<UnmergedBlocker>,
    },
}

/// Classify a ticket's unmerged direct blockers per this module's decision
/// table (see the module doc comment). `unmerged` is normally the result of
/// [`unmerged_direct_blockers`].
pub fn decide(unmerged: Vec<UnmergedBlocker>) -> StackDecision {
    match unmerged.len() {
        0 => StackDecision::Ready,
        1 => {
            let blocker = unmerged.into_iter().next().expect("len checked above");
            match &blocker.open_pr {
                Some((pr_number, head_ref_name)) => StackDecision::Stackable {
                    blocker_key: blocker.key.clone(),
                    pr_number: *pr_number,
                    head_ref_name: head_ref_name.clone(),
                },
                None => StackDecision::BlockedNoPr { blocker },
            }
        }
        _ => StackDecision::BlockedMultiple { blockers: unmerged },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jira::types::{
        IssueFields, IssueLink, IssueLinkType, LinkedIssueFields, Status, StatusCategory,
    };

    fn blocks_link_type() -> IssueLinkType {
        IssueLinkType {
            name: "Blocks".to_string(),
            inward: "is blocked by".to_string(),
            outward: "blocks".to_string(),
        }
    }

    fn linked_issue(key: &str, status_name: &str) -> LinkedIssue {
        LinkedIssue {
            key: key.to_string(),
            fields: LinkedIssueFields {
                summary: format!("Summary for {key}"),
                status: Status {
                    name: status_name.to_string(),
                    status_category: StatusCategory {
                        key: "indeterminate".to_string(),
                    },
                },
            },
        }
    }

    fn issue_blocked_by(blockers: Vec<LinkedIssue>) -> Issue {
        Issue {
            key: "PROJ-1".to_string(),
            fields: IssueFields {
                summary: "Summary for PROJ-1".to_string(),
                status: Status {
                    name: "To Do".to_string(),
                    status_category: StatusCategory {
                        key: "new".to_string(),
                    },
                },
                description: None,
                assignee: None,
                issue_links: blockers
                    .into_iter()
                    .map(|b| IssueLink {
                        id: "1".to_string(),
                        link_type: blocks_link_type(),
                        inward_issue: Some(b),
                        outward_issue: None,
                    })
                    .collect(),
            },
        }
    }

    fn pr(number: u64, head_ref_name: &str, lifecycle: PrLifecycle) -> PrSummary {
        PrSummary {
            number,
            head_ref_name: head_ref_name.to_string(),
            lifecycle,
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn decide_ready_with_no_blockers() {
        let issue = issue_blocked_by(vec![]);
        let unmerged = unmerged_direct_blockers(&issue, &[]);
        assert_eq!(decide(unmerged), StackDecision::Ready);
    }

    #[test]
    fn decide_ready_when_the_only_blocker_pr_is_merged() {
        let issue = issue_blocked_by(vec![linked_issue("AX-408", "Done")]);
        let prs = vec![pr(490, "jowi-dev/ax-408-thing", PrLifecycle::Merged)];
        let unmerged = unmerged_direct_blockers(&issue, &prs);
        assert_eq!(decide(unmerged), StackDecision::Ready);
    }

    #[test]
    fn decide_stackable_with_one_unmerged_blocker_and_open_pr() {
        let issue = issue_blocked_by(vec![linked_issue("AX-408", "Code Review")]);
        let prs = vec![pr(490, "jowi-dev/ax-408-thing", PrLifecycle::Open)];
        let unmerged = unmerged_direct_blockers(&issue, &prs);
        assert_eq!(
            decide(unmerged),
            StackDecision::Stackable {
                blocker_key: "AX-408".to_string(),
                pr_number: 490,
                head_ref_name: "jowi-dev/ax-408-thing".to_string(),
            }
        );
    }

    #[test]
    fn decide_blocked_no_pr_with_one_unmerged_blocker_and_no_pr() {
        let issue = issue_blocked_by(vec![linked_issue("AX-408", "In Progress")]);
        let unmerged = unmerged_direct_blockers(&issue, &[]);
        match decide(unmerged) {
            StackDecision::BlockedNoPr { blocker } => assert_eq!(blocker.key, "AX-408"),
            other => panic!("expected BlockedNoPr, got {other:?}"),
        }
    }

    #[test]
    fn decide_blocked_multiple_with_two_unmerged_blockers() {
        let issue = issue_blocked_by(vec![
            linked_issue("AX-408", "Code Review"),
            linked_issue("AX-409", "In Progress"),
        ]);
        let prs = vec![pr(490, "jowi-dev/ax-408-thing", PrLifecycle::Open)];
        let unmerged = unmerged_direct_blockers(&issue, &prs);
        match decide(unmerged) {
            StackDecision::BlockedMultiple { blockers } => {
                assert_eq!(blockers.len(), 2);
                assert_eq!(blockers[0].key, "AX-408");
                assert_eq!(blockers[1].key, "AX-409");
            }
            other => panic!("expected BlockedMultiple, got {other:?}"),
        }
    }

    #[test]
    fn a_closed_unmerged_pr_counts_as_no_pr_to_stack_on() {
        let issue = issue_blocked_by(vec![linked_issue("AX-408", "In Progress")]);
        let prs = vec![pr(490, "jowi-dev/ax-408-thing", PrLifecycle::Closed)];
        let unmerged = unmerged_direct_blockers(&issue, &prs);
        match decide(unmerged) {
            StackDecision::BlockedNoPr { blocker } => assert!(blocker.open_pr.is_none()),
            other => panic!("expected BlockedNoPr, got {other:?}"),
        }
    }

    #[test]
    fn find_blocker_pr_prefers_open_over_closed_among_matches() {
        let prs = vec![
            pr(1, "jowi-dev/ax-408-old", PrLifecycle::Closed),
            pr(2, "jowi-dev/ax-408-new", PrLifecycle::Open),
        ];
        let found = find_blocker_pr(&prs, "AX-408").expect("should find a match");
        assert_eq!(found.number, 2);
    }
}
