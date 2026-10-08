//! Classifying an open pull request into the overview stage it puts its
//! ticket in (ADR-0009 decision 1, ranks 2-4).

use crate::github::gh_cli::{ChecksState, PrReviewState, ReviewDecision};

/// The overview stage an open pull request puts its ticket in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrSignal {
    /// Approved, no conflicts, and checks green (or none).
    ReadyToMerge,
    /// Waiting on review: not yet approved, or approved with checks still
    /// running.
    NeedsReview,
    /// Merge conflicts with its base, or a failing check.
    Conflicted,
}

/// Classify `state`.
///
/// Conflicts and failing checks win over approval: an approved PR that
/// can't merge needs the operator just as much. Only an explicit
/// [`ReviewDecision::Approved`] counts as ready, so a repo that requires no
/// review keeps its PRs in Needs review until someone approves one.
pub fn pr_signal(state: &PrReviewState) -> PrSignal {
    if state.is_conflicting() || state.checks == ChecksState::Failing {
        return PrSignal::Conflicted;
    }
    let checks_green = matches!(state.checks, ChecksState::Passing | ChecksState::None);
    if state.review_decision == ReviewDecision::Approved && checks_green {
        PrSignal::ReadyToMerge
    } else {
        PrSignal::NeedsReview
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::pr::PrInfo;

    fn state(decision: ReviewDecision, merge: &str, checks: ChecksState) -> PrReviewState {
        PrReviewState {
            pr: PrInfo {
                number: 1,
                url: String::new(),
                title: String::new(),
                body: String::new(),
                head_ref_name: String::new(),
                base_ref_name: String::new(),
            },
            review_decision: decision,
            merge_state_status: merge.to_string(),
            checks,
        }
    }

    #[test]
    fn approved_clean_pr_with_green_or_no_checks_is_ready_to_merge() {
        for checks in [ChecksState::Passing, ChecksState::None] {
            assert_eq!(
                pr_signal(&state(ReviewDecision::Approved, "CLEAN", checks)),
                PrSignal::ReadyToMerge
            );
        }
    }

    #[test]
    fn conflicts_or_failing_checks_are_conflicted_even_when_approved() {
        assert_eq!(
            pr_signal(&state(
                ReviewDecision::Approved,
                "DIRTY",
                ChecksState::Passing
            )),
            PrSignal::Conflicted
        );
        assert_eq!(
            pr_signal(&state(
                ReviewDecision::Approved,
                "UNSTABLE",
                ChecksState::Failing
            )),
            PrSignal::Conflicted
        );
    }

    #[test]
    fn unapproved_or_still_checking_pr_needs_review() {
        for decision in [
            ReviewDecision::ReviewRequired,
            ReviewDecision::ChangesRequested,
            ReviewDecision::None,
        ] {
            assert_eq!(
                pr_signal(&state(decision, "BLOCKED", ChecksState::Passing)),
                PrSignal::NeedsReview
            );
        }
        assert_eq!(
            pr_signal(&state(
                ReviewDecision::Approved,
                "CLEAN",
                ChecksState::Pending
            )),
            PrSignal::NeedsReview
        );
    }
}
