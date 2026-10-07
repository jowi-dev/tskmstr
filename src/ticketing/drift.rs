//! Ticket/run status drift detection (GitHub issue #80).
//!
//! Every `status_on_*` transition is an advisory, one-shot move fired from a
//! specific command, so a skipped hook, a warn-and-proceed failure, or a
//! bypass path (an agent running `gh pr create` itself) leaves the tracker
//! status behind what the ticket's runs and PR actually did. Nothing
//! reconciles that after the fact; [`detect_drift`] is the safety net that
//! makes it visible. It is a pure function over a ticket's tracker status
//! category, its latest lane run, and its PR state, so the same [`Drift`]
//! value can feed `tm drift` and any other view. See
//! `docs/plans/gh-80-status-drift.md` for the full drift matrix.

use crate::github::gh_cli::PrLifecycle;
use crate::runs::RunStatus;

/// A tracker status category, from [`crate::ticketing::types::StatusCategory`]'s
/// `key`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TicketCategory {
    /// `new`: not started (e.g. To Do, Backlog).
    New,
    /// `indeterminate`: in flight (e.g. In Progress, In Review).
    InProgress,
    /// `done`: finished.
    Done,
}

impl TicketCategory {
    /// Map a status category key onto a [`TicketCategory`]. Unknown keys
    /// read as [`TicketCategory::InProgress`], the category with the fewest
    /// drift rules, so an unfamiliar workflow never manufactures a
    /// "still in To Do" finding.
    pub fn from_key(key: &str) -> Self {
        match key {
            "new" => TicketCategory::New,
            "done" => TicketCategory::Done,
            _ => TicketCategory::InProgress,
        }
    }
}

/// Everything [`detect_drift`] looks at for one ticket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriftInput {
    /// The ticket's tracker status category.
    pub category: TicketCategory,
    /// Status of the ticket's latest lane run, if it has any.
    pub run: Option<RunStatus>,
    /// Hours since the latest lane run started, if it has any.
    pub run_age_hours: Option<i64>,
    /// Lifecycle of the ticket's pull request, if one was found.
    pub pr: Option<PrLifecycle>,
}

/// A way a ticket's tracker status disagrees with its runs and PR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drift {
    /// The ticket's PR merged but the ticket is not in a done-category
    /// status. Suggested fix: `status_on_merge`.
    MergedNotDone,
    /// The latest run finished (`done`/`review`) or a PR is open, yet the
    /// ticket is still in a new-category status. Suggested fix:
    /// `status_on_pr`.
    FinishedStillNew,
    /// A run is executing (or hibernated mid-conversation) while the ticket
    /// is still in a new-category status. Suggested fix:
    /// `status_on_run_start`.
    RunningStillNew,
    /// The ticket is in progress but has no live run and no open PR, and
    /// its latest run started more than the stall threshold ago. No
    /// suggested transition: a human has to decide whether to resume,
    /// re-queue, or close it.
    Stalled,
}

impl Drift {
    /// One-line, human-readable description for listings.
    pub fn describe(self) -> &'static str {
        match self {
            Drift::MergedNotDone => "PR merged, ticket not done",
            Drift::FinishedStillNew => "work finished, ticket still not started",
            Drift::RunningStillNew => "run in progress, ticket still not started",
            Drift::Stalled => "in progress with no live run or open PR",
        }
    }
}

/// Classify `input`, returning the [`Drift`] it exhibits, if any.
///
/// Rules are checked in severity order, so a ticket matching several
/// reports the one whose fix moves it furthest: a merged PR beats a
/// finished run, which beats a running one. A done-category ticket never
/// drifts. `stall_hours` is the [`Drift::Stalled`] threshold; a ticket with
/// no runs at all has no age and is never reported stalled.
pub fn detect_drift(input: &DriftInput, stall_hours: i64) -> Option<Drift> {
    use RunStatus as R;

    if input.category == TicketCategory::Done {
        return None;
    }
    if input.pr == Some(PrLifecycle::Merged) {
        return Some(Drift::MergedNotDone);
    }

    let pr_open = input.pr == Some(PrLifecycle::Open);
    if input.category == TicketCategory::New {
        return match input.run {
            _ if pr_open => Some(Drift::FinishedStillNew),
            Some(R::Done | R::Review) => Some(Drift::FinishedStillNew),
            Some(R::Running | R::Hibernated) => Some(Drift::RunningStillNew),
            _ => None,
        };
    }

    let live = matches!(
        input.run,
        Some(R::Queued | R::Running | R::Hibernated | R::Blocked)
    );
    let stale = input.run_age_hours.is_some_and(|age| age > stall_hours);
    (!live && !pr_open && stale).then_some(Drift::Stalled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(
        category: TicketCategory,
        run: Option<RunStatus>,
        pr: Option<PrLifecycle>,
    ) -> DriftInput {
        DriftInput {
            category,
            run,
            run_age_hours: Some(1),
            pr,
        }
    }

    #[test]
    fn category_from_key_maps_jira_category_keys() {
        assert_eq!(TicketCategory::from_key("new"), TicketCategory::New);
        assert_eq!(
            TicketCategory::from_key("indeterminate"),
            TicketCategory::InProgress
        );
        assert_eq!(TicketCategory::from_key("done"), TicketCategory::Done);
        assert_eq!(
            TicketCategory::from_key("mystery"),
            TicketCategory::InProgress
        );
    }

    #[test]
    fn finished_run_with_ticket_still_new_is_finished_still_new() {
        for status in [RunStatus::Done, RunStatus::Review] {
            let i = input(TicketCategory::New, Some(status), None);
            assert_eq!(detect_drift(&i, 24), Some(Drift::FinishedStillNew));
        }
    }

    #[test]
    fn open_pr_with_ticket_still_new_is_finished_still_new() {
        let i = input(TicketCategory::New, None, Some(PrLifecycle::Open));
        assert_eq!(detect_drift(&i, 24), Some(Drift::FinishedStillNew));
    }

    #[test]
    fn interrupted_run_with_open_pr_and_ticket_new_is_finished_still_new() {
        let i = input(
            TicketCategory::New,
            Some(RunStatus::Interrupted),
            Some(PrLifecycle::Open),
        );
        assert_eq!(detect_drift(&i, 24), Some(Drift::FinishedStillNew));
    }

    #[test]
    fn merged_pr_with_ticket_not_done_is_merged_not_done() {
        for category in [TicketCategory::New, TicketCategory::InProgress] {
            let i = input(category, Some(RunStatus::Done), Some(PrLifecycle::Merged));
            assert_eq!(detect_drift(&i, 24), Some(Drift::MergedNotDone));
        }
    }

    #[test]
    fn running_or_hibernated_run_with_ticket_still_new_is_running_still_new() {
        for status in [RunStatus::Running, RunStatus::Hibernated] {
            let i = input(TicketCategory::New, Some(status), None);
            assert_eq!(detect_drift(&i, 24), Some(Drift::RunningStillNew));
        }
    }

    #[test]
    fn in_progress_with_old_dead_run_and_no_open_pr_is_stalled() {
        for status in [RunStatus::Done, RunStatus::Failed, RunStatus::Interrupted] {
            let i = DriftInput {
                category: TicketCategory::InProgress,
                run: Some(status),
                run_age_hours: Some(25),
                pr: None,
            };
            assert_eq!(detect_drift(&i, 24), Some(Drift::Stalled));
        }
    }

    #[test]
    fn in_progress_within_stall_window_is_not_stalled() {
        let i = DriftInput {
            category: TicketCategory::InProgress,
            run: Some(RunStatus::Failed),
            run_age_hours: Some(24),
            pr: None,
        };
        assert_eq!(detect_drift(&i, 24), None);
    }

    #[test]
    fn in_progress_with_live_run_or_open_pr_is_not_stalled() {
        for status in [
            RunStatus::Running,
            RunStatus::Queued,
            RunStatus::Hibernated,
            RunStatus::Blocked,
        ] {
            let i = DriftInput {
                category: TicketCategory::InProgress,
                run: Some(status),
                run_age_hours: Some(100),
                pr: None,
            };
            assert_eq!(detect_drift(&i, 24), None, "{status:?}");
        }
        let i = DriftInput {
            category: TicketCategory::InProgress,
            run: Some(RunStatus::Done),
            run_age_hours: Some(100),
            pr: Some(PrLifecycle::Open),
        };
        assert_eq!(detect_drift(&i, 24), None);
    }

    #[test]
    fn in_progress_with_no_runs_is_never_stalled() {
        let i = DriftInput {
            category: TicketCategory::InProgress,
            run: None,
            run_age_hours: None,
            pr: None,
        };
        assert_eq!(detect_drift(&i, 24), None);
    }

    #[test]
    fn done_ticket_never_drifts() {
        let i = input(
            TicketCategory::Done,
            Some(RunStatus::Running),
            Some(PrLifecycle::Open),
        );
        assert_eq!(detect_drift(&i, 24), None);
    }

    #[test]
    fn new_ticket_with_no_runs_and_no_pr_is_not_drift() {
        let i = DriftInput {
            category: TicketCategory::New,
            run: None,
            run_age_hours: None,
            pr: None,
        };
        assert_eq!(detect_drift(&i, 24), None);
    }

    #[test]
    fn new_ticket_with_failed_run_and_no_pr_is_not_drift() {
        let i = input(TicketCategory::New, Some(RunStatus::Failed), None);
        assert_eq!(detect_drift(&i, 24), None);
    }

    #[test]
    fn in_progress_with_closed_unmerged_pr_and_old_run_is_stalled() {
        let i = DriftInput {
            category: TicketCategory::InProgress,
            run: Some(RunStatus::Done),
            run_age_hours: Some(48),
            pr: Some(PrLifecycle::Closed),
        };
        assert_eq!(detect_drift(&i, 24), Some(Drift::Stalled));
    }
}
