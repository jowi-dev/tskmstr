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
//! [`unblock_counts`] sits alongside: across a whole candidate set, how much
//! downstream work each ticket frees, for ordering dispatch.
//!
//! # The rule
//!
//! For each of a ticket's *direct* `Blocks`-type Jira links, a blocker is
//! satisfied — cleared, not counted as unmerged — if EITHER of these holds:
//! - its Jira status category is `done` (no PR needed at all: work that's
//!   genuinely finished — a config change, a spike, docs, manual ops —
//!   often has no PR to find in the first place), OR
//! - it has a **merged** PR (its code is already in the base, whatever
//!   Jira's status says), OR
//! - it has a **closed** PR whose head commit is already in the lane's base
//!   branch (GitHub issue #168). A PR merged locally and pushed, then
//!   closed, never shows as merged on GitHub; a stacked PR whose own base is
//!   the blocker's branch never does either. The caller supplies the
//!   containment check ([`BaseContainment`] in production), so this stays
//!   pure. Squash and rebase merges rewrite commits and are not detected.
//!
//! Otherwise it remains unmerged, split by PR state:
//! - PR **open** → an unmerged blocker with something to stack on.
//! - **no PR** (including a closed one whose head is not in the base, or a
//!   PR whose branch name doesn't match [`find_blocker_pr`]'s naming
//!   convention) → an unmerged blocker with nothing to stack on yet.
//!
//! Then, across the ticket's unmerged blockers ([`decide`]):
//! - **zero** → [`StackDecision::Ready`].
//! - **exactly one, with an open PR** → [`StackDecision::Stackable`]: name
//!   the branch to stack on.
//! - **exactly one, with no PR, but a live lane run working it** →
//!   [`StackDecision::WaitingOnInFlight`]: its PR is probably on the way, so
//!   re-check later. [`decide`] stays pure: the caller passes the live runs
//!   ([`in_flight_lanes`]).
//! - **exactly one, with no PR and no live run** →
//!   [`StackDecision::BlockedNoPr`]: nothing to build on yet, and nobody is
//!   building it.
//! - **two or more** → [`StackDecision::BlockedMultiple`]: a single run
//!   branch can only be stacked on one dependency at a time, so this refuses
//!   rather than guessing which one.

use std::cell::Cell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::github::gh_cli::{PrLifecycle, PrSummary};
use crate::runs::{RunStatus, RunSummary};
use crate::ticketing::types::{Issue, LinkedIssue};
use crate::work::git::GitOps;

/// All direct `Blocks`-type blockers of `issue`, regardless of Jira status.
///
/// Unlike [`crate::ticketing::open_blockers`] (which itself drops blockers
/// whose Jira status is already `done` before returning), this returns every
/// direct blocker unfiltered — [`unmerged_direct_blockers`] is where the
/// done-or-merged satisfaction check actually happens, since it needs to
/// look at PR state too, not just Jira status.
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
/// already-fetched `prs`, keeping only those not satisfied — see this
/// module's doc comment for the full rule.
///
/// `in_base` answers whether a commit SHA is already in the lane's base
/// branch. It is asked only about a closed PR's non-empty head SHA; pass
/// `&|_| false` to treat every closed PR as abandoned (the pre-#168 rule).
pub fn unmerged_direct_blockers(
    issue: &Issue,
    prs: &[PrSummary],
    in_base: &dyn Fn(&str) -> bool,
) -> Vec<UnmergedBlocker> {
    direct_blockers(issue)
        .into_iter()
        .filter_map(|blocker| {
            let pr = find_blocker_pr(prs, &blocker.key);
            let is_done = blocker.fields.status.status_category.key == "done";
            let is_merged = match &pr {
                Some(pr) => match pr.lifecycle {
                    PrLifecycle::Merged => true,
                    PrLifecycle::Closed => !pr.head_ref_oid.is_empty() && in_base(&pr.head_ref_oid),
                    PrLifecycle::Open => false,
                },
                None => false,
            };
            // A blocker is cleared if EITHER condition holds, and the two
            // are independent: a blocker Done in Jira needs no stacking
            // regardless of whether it ever had a PR (config changes,
            // spikes, docs, and manual ops work often don't), and a blocker
            // with a merged PR has its code in the base regardless of
            // whether Jira's status has caught up yet. Losing either half
            // of this OR is exactly the regression this comment guards
            // against — see the module doc comment for the incident.
            if is_done || is_merged {
                return None;
            }
            let open_pr = match &pr {
                Some(pr) if pr.lifecycle == PrLifecycle::Open => {
                    Some((pr.number, pr.head_ref_name.clone()))
                }
                _ => None,
            };
            Some(UnmergedBlocker {
                key: blocker.key.clone(),
                status_name: blocker.fields.status.name.clone(),
                summary: blocker.fields.summary.clone(),
                open_pr,
            })
        })
        .collect()
}

/// The production containment check for [`unmerged_direct_blockers`]'s
/// `in_base`: whether a commit is an ancestor of any of a set of base
/// branches, compared on their `origin/` remote-tracking refs.
///
/// The first [`Self::contains`] call runs `git fetch origin` once, so a
/// ticket with no closed blocker PR never pays for a fetch. Every git
/// failure (offline fetch, a SHA or ref git doesn't have) reads as "not in
/// base", which is the rule as it was before GitHub issue #168.
pub struct BaseContainment<'a> {
    git: &'a dyn GitOps,
    dir: PathBuf,
    bases: Vec<String>,
    fetched: Cell<bool>,
}

impl<'a> BaseContainment<'a> {
    /// Check against `bases` (e.g. a lane's `base_branch`, `staging` or
    /// `origin/staging`) in the repository at `dir`.
    pub fn new(git: &'a dyn GitOps, dir: &Path, bases: impl IntoIterator<Item = String>) -> Self {
        let mut remote_bases: Vec<String> = Vec::new();
        for base in bases {
            let remote = if base.starts_with("origin/") {
                base
            } else {
                format!("origin/{base}")
            };
            if !remote_bases.contains(&remote) {
                remote_bases.push(remote);
            }
        }
        Self {
            git,
            dir: dir.to_path_buf(),
            bases: remote_bases,
            fetched: Cell::new(false),
        }
    }

    /// Whether `sha` is already in any of the base branches.
    pub fn contains(&self, sha: &str) -> bool {
        if self.bases.is_empty() {
            return false;
        }
        if !self.fetched.replace(true) {
            // A failed fetch still leaves the last-fetched refs to check.
            let _ = self.git.fetch_origin(&self.dir);
        }
        self.bases
            .iter()
            .any(|base| self.git.is_ancestor(&self.dir, sha, base).unwrap_or(false))
    }
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
    /// Exactly one unmerged direct blocker, with no PR and no live lane run
    /// working it: nothing exists to stack on, and nothing is about to.
    BlockedNoPr {
        /// The unsatisfied blocker.
        blocker: UnmergedBlocker,
    },
    /// Exactly one unmerged direct blocker with no PR yet, but a live lane
    /// run is working it (see [`in_flight_lanes`]): its PR is likely on the
    /// way, so re-check later rather than treating the blocker as unworked.
    WaitingOnInFlight {
        /// The unsatisfied blocker.
        blocker: UnmergedBlocker,
        /// The live lane run working it.
        run: InFlightRun,
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
/// [`unmerged_direct_blockers`]; `in_flight` is normally
/// [`in_flight_lanes`] over the run store, and only matters for a lone
/// blocker with no PR.
pub fn decide(unmerged: Vec<UnmergedBlocker>, in_flight: &InFlight) -> StackDecision {
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
                None => match in_flight.get(&blocker.key) {
                    Some(run) => StackDecision::WaitingOnInFlight { run: *run, blocker },
                    None => StackDecision::BlockedNoPr { blocker },
                },
            }
        }
        _ => StackDecision::BlockedMultiple { blockers: unmerged },
    }
}

/// A live lane run working a blocker: what turns [`decide`]'s
/// [`StackDecision::BlockedNoPr`] into [`StackDecision::WaitingOnInFlight`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InFlightRun {
    /// The run's row id in the run store.
    pub run_id: i64,
    /// `running`, `queued`, or `hibernated`.
    pub status: RunStatus,
}

/// Ticket key → its live lane run, as [`in_flight_lanes`] builds it.
pub type InFlight = HashMap<String, InFlightRun>;

/// Project `runs` (newest first, as [`crate::runs::RunStore::list_runs_filtered`]
/// returns them) down to each ticket's newest live `lane` run. Live means
/// `running`, `queued`, or `hibernated`: a run that can still open a PR on
/// its own or once resumed. `review` is not live, since that run has already
/// handed off.
pub fn in_flight_lanes(runs: &[RunSummary]) -> InFlight {
    let mut in_flight = InFlight::new();
    for run in runs {
        let live = matches!(
            run.status,
            RunStatus::Running | RunStatus::Queued | RunStatus::Hibernated
        );
        if run.kind == "lane" && live {
            in_flight.entry(run.ticket.clone()).or_insert(InFlightRun {
                run_id: run.id,
                status: run.status,
            });
        }
    }
    in_flight
}

/// What [`unblock_counts`] returns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnblockCounts {
    /// Every candidate's key → how many *other* candidates it blocks,
    /// directly or transitively.
    pub counts: HashMap<String, usize>,
    /// Candidates that (transitively) block themselves via a `Blocks` cycle,
    /// sorted. None of them can ever become ready until the cycle is broken.
    pub cycle_members: BTreeSet<String>,
}

/// For each of `issues`, count the other issues in the same set it blocks,
/// following `Blocks` links transitively, so a dispatcher can prefer the
/// ticket that frees the most downstream work.
///
/// Only links between two members of `issues` count: a blocker outside the
/// set gets no entry, and its own downstream is unknown anyway. A diamond's
/// bottom is counted once. A cycle terminates (each search tracks what it has
/// visited); a member counts the rest of its cycle but never itself, and is
/// listed in [`UnblockCounts::cycle_members`].
pub fn unblock_counts(issues: &[Issue]) -> UnblockCounts {
    let keys: HashSet<&str> = issues.iter().map(|i| i.key.as_str()).collect();
    let mut blocks: HashMap<&str, Vec<&str>> = HashMap::new();
    for issue in issues {
        for blocker in direct_blockers(issue) {
            if keys.contains(blocker.key.as_str()) {
                blocks
                    .entry(blocker.key.as_str())
                    .or_default()
                    .push(issue.key.as_str());
            }
        }
    }

    let mut result = UnblockCounts::default();
    for issue in issues {
        let start = issue.key.as_str();
        let mut seen: HashSet<&str> = HashSet::new();
        let mut stack: Vec<&str> = blocks.get(start).cloned().unwrap_or_default();
        while let Some(key) = stack.pop() {
            if seen.insert(key)
                && let Some(next) = blocks.get(key)
            {
                stack.extend(next);
            }
        }
        if seen.remove(start) {
            result.cycle_members.insert(start.to_string());
        }
        result.counts.insert(start.to_string(), seen.len());
    }
    result
}

/// A ticket's worker-lane readiness as the board reports it (GitHub issue
/// #62): [`decide`]'s verdict collapsed to what a card glyph needs, plus an
/// `Unknown` state for when the inputs `decide` needs couldn't be fetched.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Readiness {
    /// [`StackDecision::Ready`].
    Ready,
    /// [`StackDecision::Stackable`].
    Stackable {
        /// The one unmerged blocker whose open PR a run would stack on.
        blocker_key: String,
    },
    /// [`StackDecision::BlockedNoPr`], [`StackDecision::WaitingOnInFlight`],
    /// or [`StackDecision::BlockedMultiple`]: the board draws one glyph for
    /// all three.
    Blocked {
        /// Every unmerged blocker, in [`unmerged_direct_blockers`] order.
        blocker_keys: Vec<String>,
    },
    /// The blocker or PR lookup failed, so [`decide`] never ran. Never
    /// treated as ready: a false "ready" launches a lane on stuck work.
    #[default]
    Unknown,
}

/// Classify `issue` via [`decide`], the same rule `tm ready <KEY>` uses.
/// `prs` is the blockers' repo PR list, or `None` when that lookup failed.
/// A ticket with no direct blockers is [`Readiness::Ready`] without
/// consulting `prs` at all (matching `tm ready`, which never shells out for
/// one); a ticket with blockers and no PR list is [`Readiness::Unknown`].
pub fn readiness(issue: &Issue, prs: Option<&[PrSummary]>) -> Readiness {
    if direct_blockers(issue).is_empty() {
        return Readiness::Ready;
    }
    let Some(prs) = prs else {
        return Readiness::Unknown;
    };
    // The board shows waiting and blocked alike, so live runs are moot here.
    // It has no lane base to check against, so a closed PR reads as
    // abandoned (see GitHub issue #168).
    match decide(
        unmerged_direct_blockers(issue, prs, &|_| false),
        &InFlight::new(),
    ) {
        StackDecision::Ready => Readiness::Ready,
        StackDecision::Stackable { blocker_key, .. } => Readiness::Stackable { blocker_key },
        StackDecision::BlockedNoPr { blocker }
        | StackDecision::WaitingOnInFlight { blocker, .. } => Readiness::Blocked {
            blocker_keys: vec![blocker.key],
        },
        StackDecision::BlockedMultiple { blockers } => Readiness::Blocked {
            blocker_keys: blockers.into_iter().map(|b| b.key).collect(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ticketing::types::{
        IssueFields, IssueLink, IssueLinkType, LinkedIssueFields, Status, StatusCategory,
    };
    use crate::work::git::{FakeGitOps, GitError};

    fn blocks_link_type() -> IssueLinkType {
        IssueLinkType {
            name: "Blocks".to_string(),
            inward: "is blocked by".to_string(),
            outward: "blocks".to_string(),
        }
    }

    fn linked_issue(key: &str, status_name: &str) -> LinkedIssue {
        linked_issue_with_category(key, status_name, "indeterminate")
    }

    fn linked_issue_with_category(
        key: &str,
        status_name: &str,
        status_category_key: &str,
    ) -> LinkedIssue {
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
                parent: None,
                labels: Vec::new(),
                issue_type: None,
            },
        }
    }

    fn pr(number: u64, head_ref_name: &str, lifecycle: PrLifecycle) -> PrSummary {
        PrSummary {
            number,
            head_ref_name: head_ref_name.to_string(),
            head_ref_oid: String::new(),
            lifecycle,
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn decide_ready_with_no_blockers() {
        let issue = issue_blocked_by(vec![]);
        let unmerged = unmerged_direct_blockers(&issue, &[], &never_landed);
        assert_eq!(decide(unmerged, &no_runs()), StackDecision::Ready);
    }

    #[test]
    fn decide_ready_when_the_only_blocker_pr_is_merged() {
        let issue = issue_blocked_by(vec![linked_issue("AX-408", "Done")]);
        let prs = vec![pr(490, "jowi-dev/ax-408-thing", PrLifecycle::Merged)];
        let unmerged = unmerged_direct_blockers(&issue, &prs, &never_landed);
        assert_eq!(decide(unmerged, &no_runs()), StackDecision::Ready);
    }

    /// A blocker that's genuinely Done in Jira but has no discoverable PR at
    /// all (a config change, a spike, docs, manual ops work — plenty of real
    /// tickets never have a PR) must still clear the blocker. This is the
    /// half of the rule that regressed when `unmerged_direct_blockers` was
    /// extracted: only the merged-PR path cleared a blocker, so a Done
    /// blocker with no PR fell into the catch-all "unmerged, no PR" arm and
    /// wrongly stayed blocking.
    #[test]
    fn decide_ready_when_the_only_blocker_is_done_in_jira_with_no_pr_at_all() {
        let issue = issue_blocked_by(vec![linked_issue_with_category("AX-408", "Done", "done")]);
        let unmerged = unmerged_direct_blockers(&issue, &[], &never_landed);
        assert_eq!(decide(unmerged, &no_runs()), StackDecision::Ready);
    }

    /// Pins the exact incident this module's doc comment describes: AX-408
    /// sat in Jira status "Code Review" (status category `indeterminate`,
    /// NOT `done`) with an open PR #490, and AX-409/AX-436 must resolve to
    /// `Stackable` on it rather than `BlockedNoPr`/`BlockedMultiple`. If a
    /// future change makes Jira status alone (without checking the PR)
    /// decide satisfaction, this is the test that catches it.
    #[test]
    fn ax_408_in_code_review_with_open_pr_is_stackable_not_blocked() {
        let issue = issue_blocked_by(vec![linked_issue_with_category(
            "AX-408",
            "Code Review",
            "indeterminate",
        )]);
        let prs = vec![pr(490, "jowi-dev/ax-408-thing", PrLifecycle::Open)];
        let unmerged = unmerged_direct_blockers(&issue, &prs, &never_landed);
        assert_eq!(
            decide(unmerged, &no_runs()),
            StackDecision::Stackable {
                blocker_key: "AX-408".to_string(),
                pr_number: 490,
                head_ref_name: "jowi-dev/ax-408-thing".to_string(),
            }
        );
    }

    #[test]
    fn decide_stackable_with_one_unmerged_blocker_and_open_pr() {
        let issue = issue_blocked_by(vec![linked_issue("AX-408", "Code Review")]);
        let prs = vec![pr(490, "jowi-dev/ax-408-thing", PrLifecycle::Open)];
        let unmerged = unmerged_direct_blockers(&issue, &prs, &never_landed);
        assert_eq!(
            decide(unmerged, &no_runs()),
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
        let unmerged = unmerged_direct_blockers(&issue, &[], &never_landed);
        match decide(unmerged, &no_runs()) {
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
        let unmerged = unmerged_direct_blockers(&issue, &prs, &never_landed);
        match decide(unmerged, &no_runs()) {
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
        let unmerged = unmerged_direct_blockers(&issue, &prs, &never_landed);
        match decide(unmerged, &no_runs()) {
            StackDecision::BlockedNoPr { blocker } => assert!(blocker.open_pr.is_none()),
            other => panic!("expected BlockedNoPr, got {other:?}"),
        }
    }

    fn pr_with_head(
        number: u64,
        head_ref_name: &str,
        head_ref_oid: &str,
        lifecycle: PrLifecycle,
    ) -> PrSummary {
        PrSummary {
            head_ref_oid: head_ref_oid.to_string(),
            ..pr(number, head_ref_name, lifecycle)
        }
    }

    /// A containment check that reports exactly `shas` as in the base branch.
    fn landed<'a>(shas: &'a [&'a str]) -> impl Fn(&str) -> bool + 'a {
        move |sha| shas.contains(&sha)
    }

    fn never_landed(_sha: &str) -> bool {
        false
    }

    /// GitHub issue #168: a PR closed without merging, whose commits reached
    /// the base branch anyway (merged locally, then pushed), is merged.
    #[test]
    fn a_closed_pr_whose_head_is_in_the_base_branch_clears_the_blocker() {
        let issue = issue_blocked_by(vec![linked_issue("AX-711", "In Review")]);
        let prs = vec![pr_with_head(
            673,
            "jowi-dev/ax-711-thing",
            "aaa111",
            PrLifecycle::Closed,
        )];
        let unmerged = unmerged_direct_blockers(&issue, &prs, &landed(&["aaa111"]));
        assert_eq!(decide(unmerged, &no_runs()), StackDecision::Ready);
    }

    /// The stacked shape: AX-712's PR targeted AX-711's branch, not the lane
    /// base, so GitHub never saw it merge. Only its head reaching the lane
    /// base matters, which the containment check answers.
    #[test]
    fn a_closed_stacked_pr_whose_head_is_in_the_lane_base_clears_the_blocker() {
        let issue = issue_blocked_by(vec![linked_issue("AX-712", "In Review")]);
        let prs = vec![
            pr_with_head(673, "jowi-dev/ax-711-thing", "aaa111", PrLifecycle::Closed),
            pr_with_head(696, "jowi-dev/ax-712-thing", "bbb222", PrLifecycle::Closed),
        ];
        let unmerged = unmerged_direct_blockers(&issue, &prs, &landed(&["aaa111", "bbb222"]));
        assert_eq!(decide(unmerged, &no_runs()), StackDecision::Ready);
    }

    #[test]
    fn an_abandoned_closed_pr_whose_head_is_not_in_the_base_stays_blocked() {
        let issue = issue_blocked_by(vec![linked_issue("AX-711", "In Progress")]);
        let prs = vec![pr_with_head(
            673,
            "jowi-dev/ax-711-thing",
            "aaa111",
            PrLifecycle::Closed,
        )];
        let unmerged = unmerged_direct_blockers(&issue, &prs, &landed(&["other"]));
        match decide(unmerged, &no_runs()) {
            StackDecision::BlockedNoPr { blocker } => assert!(blocker.open_pr.is_none()),
            other => panic!("expected BlockedNoPr, got {other:?}"),
        }
    }

    /// The AX-713 incident from GitHub issue #168: both blockers' PRs were
    /// closed unmerged after their commits were pushed to staging.
    #[test]
    fn two_closed_blockers_both_in_the_base_are_ready_not_blocked_multiple() {
        let issue = issue_blocked_by(vec![
            linked_issue("AX-711", "In Review"),
            linked_issue("AX-712", "In Review"),
        ]);
        let prs = vec![
            pr_with_head(673, "jowi-dev/ax-711-thing", "aaa111", PrLifecycle::Closed),
            pr_with_head(696, "jowi-dev/ax-712-thing", "bbb222", PrLifecycle::Closed),
        ];
        let unmerged = unmerged_direct_blockers(&issue, &prs, &landed(&["aaa111", "bbb222"]));
        assert_eq!(decide(unmerged, &no_runs()), StackDecision::Ready);
    }

    /// Only a closed PR is checked: an open PR stays stackable, not merged,
    /// even when its head already sits in the base.
    #[test]
    fn an_open_pr_whose_head_is_in_the_base_stays_stackable() {
        let issue = issue_blocked_by(vec![linked_issue("AX-711", "Code Review")]);
        let prs = vec![pr_with_head(
            673,
            "jowi-dev/ax-711-thing",
            "aaa111",
            PrLifecycle::Open,
        )];
        let unmerged = unmerged_direct_blockers(&issue, &prs, &landed(&["aaa111"]));
        assert!(matches!(
            decide(unmerged, &no_runs()),
            StackDecision::Stackable { .. }
        ));
    }

    #[test]
    fn a_closed_pr_with_no_head_sha_never_asks_the_containment_check() {
        let issue = issue_blocked_by(vec![linked_issue("AX-711", "In Progress")]);
        let prs = vec![pr(673, "jowi-dev/ax-711-thing", PrLifecycle::Closed)];
        let unmerged = unmerged_direct_blockers(&issue, &prs, &|_: &str| -> bool {
            panic!("an empty SHA must not be checked")
        });
        assert_eq!(unmerged.len(), 1);
    }

    fn git_error() -> GitError {
        GitError::Command {
            command: "git".to_string(),
            exit_code: Some(128),
            stderr: "fatal".to_string(),
        }
    }

    #[test]
    fn base_containment_checks_the_remote_tracking_ref_of_each_base() {
        let git = FakeGitOps::new()
            .with_is_ancestor_result(Ok(false))
            .with_is_ancestor_for("aaa111", "origin/staging", Ok(true));
        let check = BaseContainment::new(&git, Path::new("/repo"), ["staging".to_string()]);
        assert!(check.contains("aaa111"));
        assert!(!check.contains("bbb222"));
    }

    #[test]
    fn base_containment_keeps_an_already_remote_base_as_is() {
        let git = FakeGitOps::new()
            .with_is_ancestor_result(Ok(false))
            .with_is_ancestor_for("aaa111", "origin/main", Ok(true));
        let check = BaseContainment::new(&git, Path::new("/repo"), ["origin/main".to_string()]);
        assert!(check.contains("aaa111"));
    }

    #[test]
    fn base_containment_fetches_once_and_only_when_asked() {
        let git = FakeGitOps::new().with_is_ancestor_result(Ok(false));
        let check = BaseContainment::new(&git, Path::new("/repo"), ["staging".to_string()]);
        assert!(git.fetch_origin_calls().is_empty());
        check.contains("aaa111");
        check.contains("bbb222");
        assert_eq!(git.fetch_origin_calls(), vec![PathBuf::from("/repo")]);
    }

    /// GitHub issue #168's safe-failure criterion: no network, or a SHA or
    /// ref git doesn't have, reads as "not in base", today's behaviour.
    #[test]
    fn base_containment_treats_git_failures_as_not_in_base() {
        let git = FakeGitOps::new()
            .with_fetch_origin_result(Err(git_error()))
            .with_is_ancestor_result(Err(git_error()));
        let check = BaseContainment::new(&git, Path::new("/repo"), ["staging".to_string()]);
        assert!(!check.contains("aaa111"));
    }

    #[test]
    fn base_containment_with_no_bases_never_touches_git() {
        let git = FakeGitOps::new();
        let check = BaseContainment::new(&git, Path::new("/repo"), Vec::<String>::new());
        assert!(!check.contains("aaa111"));
        assert!(git.fetch_origin_calls().is_empty());
    }

    #[test]
    fn readiness_with_no_blockers_is_ready_even_without_a_pr_list() {
        let issue = issue_blocked_by(vec![]);
        assert_eq!(readiness(&issue, None), Readiness::Ready);
    }

    #[test]
    fn readiness_with_a_blocker_but_no_pr_list_is_unknown_never_ready() {
        let issue = issue_blocked_by(vec![linked_issue("AX-408", "In Progress")]);
        assert_eq!(readiness(&issue, None), Readiness::Unknown);
    }

    #[test]
    fn readiness_maps_each_decision() {
        let stackable = issue_blocked_by(vec![linked_issue("AX-408", "Code Review")]);
        let prs = vec![pr(490, "jowi-dev/ax-408-thing", PrLifecycle::Open)];
        assert_eq!(
            readiness(&stackable, Some(&prs)),
            Readiness::Stackable {
                blocker_key: "AX-408".to_string()
            }
        );

        let blocked_no_pr = issue_blocked_by(vec![linked_issue("AX-408", "In Progress")]);
        assert_eq!(
            readiness(&blocked_no_pr, Some(&[])),
            Readiness::Blocked {
                blocker_keys: vec!["AX-408".to_string()]
            }
        );

        let blocked_multiple = issue_blocked_by(vec![
            linked_issue("AX-408", "Code Review"),
            linked_issue("AX-409", "In Progress"),
        ]);
        assert_eq!(
            readiness(&blocked_multiple, Some(&prs)),
            Readiness::Blocked {
                blocker_keys: vec!["AX-408".to_string(), "AX-409".to_string()]
            }
        );

        let merged = issue_blocked_by(vec![linked_issue("AX-408", "Done")]);
        let merged_prs = vec![pr(490, "jowi-dev/ax-408-thing", PrLifecycle::Merged)];
        assert_eq!(readiness(&merged, Some(&merged_prs)), Readiness::Ready);
    }

    fn no_runs() -> InFlight {
        InFlight::new()
    }

    fn in_flight(key: &str, run_id: i64, status: RunStatus) -> InFlight {
        InFlight::from([(key.to_string(), InFlightRun { run_id, status })])
    }

    #[test]
    fn decide_waits_on_a_no_pr_blocker_with_a_live_lane_run() {
        let issue = issue_blocked_by(vec![linked_issue("AX-408", "In Progress")]);
        let unmerged = unmerged_direct_blockers(&issue, &[], &never_landed);
        let runs = in_flight("AX-408", 7, RunStatus::Running);
        match decide(unmerged, &runs) {
            StackDecision::WaitingOnInFlight { blocker, run } => {
                assert_eq!(blocker.key, "AX-408");
                assert_eq!(
                    run,
                    InFlightRun {
                        run_id: 7,
                        status: RunStatus::Running
                    }
                );
            }
            other => panic!("expected WaitingOnInFlight, got {other:?}"),
        }
    }

    #[test]
    fn decide_stays_blocked_no_pr_when_another_ticket_is_in_flight() {
        let issue = issue_blocked_by(vec![linked_issue("AX-408", "In Progress")]);
        let unmerged = unmerged_direct_blockers(&issue, &[], &never_landed);
        let runs = in_flight("AX-999", 7, RunStatus::Running);
        assert!(matches!(
            decide(unmerged, &runs),
            StackDecision::BlockedNoPr { .. }
        ));
    }

    #[test]
    fn decide_prefers_stacking_over_waiting_once_the_blocker_has_an_open_pr() {
        let issue = issue_blocked_by(vec![linked_issue("AX-408", "Code Review")]);
        let prs = vec![pr(490, "jowi-dev/ax-408-thing", PrLifecycle::Open)];
        let unmerged = unmerged_direct_blockers(&issue, &prs, &never_landed);
        let runs = in_flight("AX-408", 7, RunStatus::Running);
        assert!(matches!(
            decide(unmerged, &runs),
            StackDecision::Stackable { .. }
        ));
    }

    fn summary(id: i64, ticket: &str, kind: &str, status: RunStatus) -> RunSummary {
        RunSummary {
            id,
            ticket: ticket.to_string(),
            lane: "default".to_string(),
            kind: kind.to_string(),
            status,
            age_secs: 0,
            heartbeat_age_secs: None,
            last_event_kind: None,
            last_event_age_secs: None,
            awaiting_input: false,
            scope: String::new(),
            mem_current_bytes: None,
            mem_peak_bytes: None,
        }
    }

    #[test]
    fn in_flight_lanes_keeps_live_lane_runs_only() {
        let runs = vec![
            summary(1, "AX-1", "lane", RunStatus::Running),
            summary(2, "AX-2", "lane", RunStatus::Queued),
            summary(3, "AX-3", "lane", RunStatus::Hibernated),
            summary(4, "AX-4", "lane", RunStatus::Done),
            summary(5, "AX-5", "lane", RunStatus::Review),
            summary(6, "AX-6", "audit", RunStatus::Running),
        ];
        let live = in_flight_lanes(&runs);
        let mut keys: Vec<&str> = live.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["AX-1", "AX-2", "AX-3"]);
        assert_eq!(live["AX-3"].status, RunStatus::Hibernated);
    }

    #[test]
    fn in_flight_lanes_keeps_the_newest_live_run_per_ticket() {
        // `list_runs_filtered` orders newest first.
        let runs = vec![
            summary(9, "AX-1", "lane", RunStatus::Running),
            summary(4, "AX-1", "lane", RunStatus::Hibernated),
        ];
        assert_eq!(in_flight_lanes(&runs)["AX-1"].run_id, 9);
    }

    /// An issue keyed `key` blocked by each of `blockers`.
    fn keyed(key: &str, blockers: &[&str]) -> Issue {
        let mut issue =
            issue_blocked_by(blockers.iter().map(|b| linked_issue(b, "To Do")).collect());
        issue.key = key.to_string();
        issue
    }

    #[test]
    fn unblock_counts_follows_a_chain_transitively() {
        let issues = vec![keyed("A", &[]), keyed("B", &["A"]), keyed("C", &["B"])];
        let counts = unblock_counts(&issues);
        assert_eq!(counts.counts["A"], 2);
        assert_eq!(counts.counts["B"], 1);
        assert_eq!(counts.counts["C"], 0);
        assert!(counts.cycle_members.is_empty());
    }

    #[test]
    fn unblock_counts_counts_a_diamond_bottom_once() {
        let issues = vec![
            keyed("A", &[]),
            keyed("B", &["A"]),
            keyed("C", &["A"]),
            keyed("D", &["B", "C"]),
        ];
        let counts = unblock_counts(&issues);
        assert_eq!(counts.counts["A"], 3);
        assert_eq!(counts.counts["B"], 1);
        assert_eq!(counts.counts["D"], 0);
    }

    #[test]
    fn unblock_counts_ignores_blockers_outside_the_candidate_set() {
        let issues = vec![keyed("A", &["OUTSIDE"]), keyed("B", &["A"])];
        let counts = unblock_counts(&issues);
        assert_eq!(counts.counts["A"], 1);
        assert!(!counts.counts.contains_key("OUTSIDE"));
    }

    #[test]
    fn unblock_counts_terminates_on_a_cycle_and_reports_its_members() {
        let issues = vec![
            keyed("A", &["C"]),
            keyed("B", &["A"]),
            keyed("C", &["B"]),
            keyed("D", &["C"]),
        ];
        let counts = unblock_counts(&issues);
        // Each cycle member unblocks the other two plus D, never itself.
        assert_eq!(counts.counts["A"], 3);
        assert_eq!(counts.counts["D"], 0);
        assert_eq!(
            counts
                .cycle_members
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["A", "B", "C"]
        );
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
