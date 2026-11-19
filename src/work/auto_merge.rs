//! Merging a lane's pull request without the operator (GitHub issue #243).
//!
//! With `[dispatch] auto_merge` set ([`AutoMerge`]), each dispatch tick
//! runs [`sweep`] after the CI-fix sweep. It picks one ticket whose review
//! is complete and starts a detached `tm merge` for it:
//!
//! - [`AutoMerge::Approved`] merges at the overview's Ready to merge: an
//!   explicit approval, no conflicts, green checks ([`pr_signal`]).
//! - [`AutoMerge::Reviewed`] also merges at Awaiting review
//!   ([`awaiting_review`]): the bots and auto-review are done, nothing is
//!   running, and no human has looked at it.
//!
//! A PR with a failing check is never picked; the CI-fix sweep owns it.
//!
//! **One merge per scope at a time.** Each merge rebases onto the base the
//! previous one changed, so a sweep starts nothing while the scope has a
//! live [`AUTO_MERGE_KIND`] run or a live `tm merge` conflict session.
//!
//! **One merge per head commit.** Before it launches, the sweep records an
//! [`AUTO_MERGE_EVENT`] on the ticket's lane run naming the PR's head SHA,
//! so the next tick does not start a second merge while the first is still
//! starting. A failed merge is not retried until the PR's head changes, in
//! either mode: the sweep leaves its own rows out of [`awaiting_review`].
//!
//! Each merge gets its own [`AUTO_MERGE_KIND`] run row, registered by the
//! sweep and adopted by the detached `tm merge --auto-merge-run <ID>`,
//! which ends it with [`finish`]. The row is what makes a running or failed
//! automerge visible in `tm runs`, the overview and `tm dispatch status`,
//! and a row whose process died is reaped like any other run.
//! A merge that does not land ends the row `failed` and flags the ticket's
//! lane run for review; [`crate::work::notify::ticket_notices`] then
//! notifies once per failed row.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use crate::config::AutoMerge;
use crate::github::gh_cli::{ChecksState, GhCli, PrLifecycle, PrReviewState};
use crate::github::pr::pr_number_from_url;
use crate::overview_poll::signal::{PrSignal, pr_signal};
use crate::runs::overview::{OverviewRun, awaiting_review, is_live};
use crate::runs::{FinishRun, RunEvent, RunStatus, RunStore, RunStoreError, StartRun};
use crate::work::merge::{MERGE_CONFLICT_KIND, MergeFlowOutcome};

/// `kind` (and `lane`) of an automerge's run row.
pub const AUTO_MERGE_KIND: &str = "auto-merge";

/// Event kind recorded on the ticket's lane run. Detail is
/// `started <sha> run <id>` or `failed <sha>: <error>`.
pub const AUTO_MERGE_EVENT: &str = "auto_merge";

/// How long the sweep waits for `gh pr list`.
const GH_TIMEOUT: Duration = Duration::from_secs(30);

/// Whether `mode` merges a PR in `state`. `awaiting` is
/// [`awaiting_review`] for the ticket's runs. A failing check never
/// qualifies.
pub fn eligible(mode: AutoMerge, state: &PrReviewState, awaiting: bool) -> bool {
    if state.checks == ChecksState::Failing {
        return false;
    }
    let ready = pr_signal(state) == PrSignal::ReadyToMerge;
    match mode {
        AutoMerge::Off => false,
        AutoMerge::Approved => ready,
        AutoMerge::Reviewed => ready || awaiting,
    }
}

/// Whether `events` (a lane run's) hold an [`AUTO_MERGE_EVENT`] for head
/// commit `sha`, whatever its outcome.
pub fn handled(events: &[RunEvent], sha: &str) -> bool {
    crate::work::ci_fix::handled_by(events, AUTO_MERGE_EVENT, sha)
}

/// Starts the detached `tm merge` for one automerge.
pub trait AutoMergeLauncher {
    /// Starts merging `ticket` under run row `run_id`, returning the
    /// process's pid.
    ///
    /// # Errors
    ///
    /// Why the merge could not be started, as text for the run's blocker.
    fn launch(&self, ticket: &str, run_id: i64) -> Result<u32, String>;
}

/// What [`sweep`] needs besides the runs it inspects.
pub struct SweepDeps<'a> {
    /// The run store holding the lane runs and the automerge rows.
    pub store: &'a RunStore,
    /// Reads the PRs' review state and head commits.
    pub gh: &'a dyn GhCli,
    /// Where `gh` runs: a checkout of the scope's repo.
    pub repo_dir: &'a Path,
    /// Starts the merge.
    pub launcher: &'a dyn AutoMergeLauncher,
}

/// What [`sweep`] did. A sweep that started nothing returns none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoMergeAction {
    /// A merge was started for head commit `sha` under run row `run_id`.
    Started {
        /// Ticket key.
        ticket: String,
        /// The PR's head commit.
        sha: String,
        /// The automerge's run row.
        run_id: i64,
    },
    /// The launcher refused; the run row ended `failed` and the lane run
    /// was flagged for review.
    LaunchFailed {
        /// Ticket key.
        ticket: String,
        /// The PR's head commit.
        sha: String,
        /// The launcher's error.
        error: String,
    },
    /// `gh` could not read the PRs; retried next sweep.
    CheckFailed {
        /// The ticket whose PR could not be read, or `""` when the scope's
        /// PR list could not be.
        ticket: String,
        /// The `gh` error.
        error: String,
    },
}

/// One dispatch tick's automerge pass over `scope`: starts at most one
/// `tm merge`, for the open lane PR with the lowest number that `mode`
/// finds [`eligible`] on a head commit not yet [`handled`].
///
/// `runs` is [`RunStore::overview_runs`] output, newest first. Nothing
/// starts while the scope has a live [`AUTO_MERGE_KIND`] or
/// [`MERGE_CONFLICT_KIND`] run. A ticket is skipped while any of its runs
/// is live, once any run recorded its PR closed, and when it has no lane
/// run with a PR.
///
/// # Errors
///
/// Any run-store failure. A `gh` failure is a [`AutoMergeAction::CheckFailed`]
/// and a launch failure a [`AutoMergeAction::LaunchFailed`].
pub fn sweep(
    deps: &SweepDeps<'_>,
    runs: &[OverviewRun],
    scope: &str,
    mode: AutoMerge,
) -> Result<Vec<AutoMergeAction>, RunStoreError> {
    if mode == AutoMerge::Off {
        return Ok(Vec::new());
    }
    let scoped: Vec<&OverviewRun> = runs.iter().filter(|run| run.scope == scope).collect();
    let merging = scoped.iter().any(|run| {
        is_live(run.status) && [AUTO_MERGE_KIND, MERGE_CONFLICT_KIND].contains(&run.kind.as_str())
    });
    if merging {
        return Ok(Vec::new());
    }

    let mut by_ticket: Vec<(&str, Vec<&OverviewRun>)> = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();
    for run in scoped {
        let at = *index.entry(&run.ticket).or_insert_with(|| {
            by_ticket.push((&run.ticket, Vec::new()));
            by_ticket.len() - 1
        });
        by_ticket[at].1.push(run);
    }

    // (PR number, ticket, lane run, awaiting review)
    let mut candidates: Vec<(u64, &str, &OverviewRun, bool)> = Vec::new();
    for (ticket, runs) in &by_ticket {
        if runs.iter().any(|run| is_live(run.status)) || runs.iter().any(|run| run.pr_closed) {
            continue;
        }
        let Some((lane, number)) = runs.iter().find_map(|run| {
            let url = run.pr_url.as_deref().filter(|_| run.kind == "lane")?;
            Some((*run, pr_number_from_url(url)?))
        }) else {
            continue;
        };
        // A failed automerge row would otherwise count as a failure after
        // the review watch and hold the ticket out of Awaiting review for
        // good. `handled` already keeps a failed merge from retrying on the
        // same head, so the sweep leaves its own rows out here.
        let reviewed: Vec<&OverviewRun> = runs
            .iter()
            .copied()
            .filter(|run| run.kind != AUTO_MERGE_KIND)
            .collect();
        candidates.push((number, ticket, lane, awaiting_review(&reviewed)));
    }
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    candidates.sort_by_key(|(number, ..)| *number);

    let open = match deps.gh.pr_list_review_state(deps.repo_dir, GH_TIMEOUT) {
        Ok(open) => open,
        Err(err) => {
            return Ok(vec![AutoMergeAction::CheckFailed {
                ticket: String::new(),
                error: err.to_string(),
            }]);
        }
    };

    let mut actions = Vec::new();
    for (number, ticket, lane, awaiting) in candidates {
        let Some(state) = open.iter().find(|state| state.pr.number == number) else {
            continue;
        };
        if !eligible(mode, state, awaiting) {
            continue;
        }
        let sha = match deps.gh.pr_checks(deps.repo_dir, number) {
            Ok(checks) if checks.lifecycle == PrLifecycle::Open => checks.head_sha,
            Ok(_) => continue,
            Err(err) => {
                actions.push(AutoMergeAction::CheckFailed {
                    ticket: ticket.to_string(),
                    error: err.to_string(),
                });
                continue;
            }
        };
        // An empty SHA would make every event look unhandled.
        if sha.is_empty() || handled(&deps.store.events_for_run(lane.id)?, &sha) {
            continue;
        }
        actions.push(start(deps, scope, ticket, lane, sha)?);
        break;
    }
    Ok(actions)
}

/// Registers the automerge's run row, records the head as handled on the
/// lane run, and launches the merge.
fn start(
    deps: &SweepDeps<'_>,
    scope: &str,
    ticket: &str,
    lane: &OverviewRun,
    sha: String,
) -> Result<AutoMergeAction, RunStoreError> {
    let run_id = deps.store.start_run(&StartRun {
        ticket: ticket.to_string(),
        scope: scope.to_string(),
        lane: AUTO_MERGE_KIND.to_string(),
        worktree: lane.worktree.clone(),
        branch: None,
        pid: None,
        kind: AUTO_MERGE_KIND.to_string(),
        log_path: None,
    })?;
    // Recorded before the launch, so the next tick sees this head as
    // handled whatever becomes of the process.
    deps.store.add_event(
        lane.id,
        AUTO_MERGE_EVENT,
        Some(&format!("started {sha} run {run_id}")),
    )?;
    match deps.launcher.launch(ticket, run_id) {
        Ok(pid) => {
            deps.store.update_pid(run_id, pid)?;
            Ok(AutoMergeAction::Started {
                ticket: ticket.to_string(),
                sha,
                run_id,
            })
        }
        Err(error) => {
            deps.store.add_event(
                lane.id,
                AUTO_MERGE_EVENT,
                Some(&format!("failed {sha}: {error}")),
            )?;
            finish(deps.store, run_id, &Err(error.clone()))?;
            Ok(AutoMergeAction::LaunchFailed {
                ticket: ticket.to_string(),
                sha,
                error,
            })
        }
    }
}

/// Ends automerge run `run_id` with the detached `tm merge`'s `result`:
/// `done` when it merged, otherwise `failed` with the reason as its blocker
/// and the ticket's newest lane run flagged for review.
///
/// # Errors
///
/// Any run-store failure.
pub fn finish(
    store: &RunStore,
    run_id: i64,
    result: &Result<MergeFlowOutcome, String>,
) -> Result<(), RunStoreError> {
    let blocker = match result {
        Ok(MergeFlowOutcome::Merged) => None,
        Ok(MergeFlowOutcome::ConflictsHandedBack) => Some(
            "conflicts handed back: resolve them in the merge tmux window, then run `tm merge`"
                .to_string(),
        ),
        Ok(MergeFlowOutcome::ChecksPending) => Some(
            "checks still running when [work.merge] checks_timeout_secs ran out; not merged"
                .to_string(),
        ),
        Err(error) => Some(error.clone()),
    };
    let status = if blocker.is_some() {
        RunStatus::Failed
    } else {
        RunStatus::Done
    };
    store.finish_run(
        run_id,
        &FinishRun {
            status,
            blocker: blocker.clone(),
            ..FinishRun::default()
        },
    )?;
    if blocker.is_none() {
        return Ok(());
    }
    let Some(run) = store.run_by_id(run_id)? else {
        return Ok(());
    };
    if let Some(lane) =
        store.latest_run_for_ticket_kind(Some(&run.scope), &run.ticket, Some("lane"))?
    {
        store.flag_for_review(lane.id)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::gh_cli::{FakeGhCli, GhError, PrChecks, ReviewDecision};
    use crate::github::pr::PrInfo;
    use std::cell::RefCell;
    use tempfile::TempDir;

    const SCOPE: &str = "gh:o/r";

    fn review_state(number: u64, decision: ReviewDecision, checks: ChecksState) -> PrReviewState {
        PrReviewState {
            pr: PrInfo {
                number,
                url: format!("https://github.com/o/r/pull/{number}"),
                title: String::new(),
                body: String::new(),
                head_ref_name: String::new(),
                base_ref_name: String::new(),
            },
            review_decision: decision,
            merge_state_status: "CLEAN".to_string(),
            checks,
        }
    }

    fn approved(number: u64) -> PrReviewState {
        review_state(number, ReviewDecision::Approved, ChecksState::Passing)
    }

    fn unapproved(number: u64) -> PrReviewState {
        review_state(number, ReviewDecision::ReviewRequired, ChecksState::Passing)
    }

    fn head(sha: &str) -> PrChecks {
        PrChecks {
            lifecycle: PrLifecycle::Open,
            head_sha: sha.to_string(),
            checks: Vec::new(),
        }
    }

    // --- eligible ---

    #[test]
    fn off_never_merges() {
        assert!(!eligible(AutoMerge::Off, &approved(1), true));
    }

    #[test]
    fn approved_merges_only_a_ready_to_merge_pr() {
        assert!(eligible(AutoMerge::Approved, &approved(1), false));
        assert!(!eligible(AutoMerge::Approved, &unapproved(1), true));
        let pending = review_state(1, ReviewDecision::Approved, ChecksState::Pending);
        assert!(!eligible(AutoMerge::Approved, &pending, false));
    }

    #[test]
    fn reviewed_also_merges_an_awaiting_review_pr() {
        assert!(eligible(AutoMerge::Reviewed, &approved(1), false));
        assert!(eligible(AutoMerge::Reviewed, &unapproved(1), true));
        assert!(!eligible(AutoMerge::Reviewed, &unapproved(1), false));
    }

    #[test]
    fn a_failing_check_is_never_eligible() {
        let red = review_state(1, ReviewDecision::ReviewRequired, ChecksState::Failing);
        assert!(!eligible(AutoMerge::Reviewed, &red, true));
    }

    // --- sweep ---

    struct FakeLauncher {
        launched: RefCell<Vec<(String, i64)>>,
        error: Option<String>,
    }

    impl FakeLauncher {
        fn new() -> Self {
            Self {
                launched: RefCell::new(Vec::new()),
                error: None,
            }
        }

        fn failing(error: &str) -> Self {
            Self {
                error: Some(error.to_string()),
                ..Self::new()
            }
        }

        fn tickets(&self) -> Vec<String> {
            self.launched
                .borrow()
                .iter()
                .map(|(ticket, _)| ticket.clone())
                .collect()
        }
    }

    impl AutoMergeLauncher for FakeLauncher {
        fn launch(&self, ticket: &str, run_id: i64) -> Result<u32, String> {
            self.launched
                .borrow_mut()
                .push((ticket.to_string(), run_id));
            match &self.error {
                Some(error) => Err(error.clone()),
                None => Ok(4242),
            }
        }
    }

    struct Fixture {
        _tmp: TempDir,
        store: RunStore,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = TempDir::new().unwrap();
            let store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
            Self { _tmp: tmp, store }
        }

        fn start(&self, ticket: &str, kind: &str) -> i64 {
            self.store
                .start_run(&StartRun {
                    ticket: ticket.to_string(),
                    scope: SCOPE.to_string(),
                    lane: "backend".to_string(),
                    worktree: format!("/wt/{ticket}"),
                    branch: None,
                    pid: None,
                    kind: kind.to_string(),
                    log_path: None,
                })
                .unwrap()
        }

        fn end(&self, run_id: i64, status: RunStatus, pr_url: Option<String>) {
            self.store
                .finish_run(
                    run_id,
                    &FinishRun {
                        status,
                        pr_url,
                        ..FinishRun::default()
                    },
                )
                .unwrap();
        }

        /// A finished lane run for `ticket` with PR `number`.
        fn lane(&self, ticket: &str, number: u64) -> i64 {
            let id = self.start(ticket, "lane");
            self.end(
                id,
                RunStatus::Done,
                Some(format!("https://github.com/o/r/pull/{number}")),
            );
            id
        }

        /// A finished lane plus a clean review watch: awaiting review.
        fn awaiting(&self, ticket: &str, number: u64) -> i64 {
            let lane = self.lane(ticket, number);
            let watch = self.start(ticket, "review-watch");
            self.end(watch, RunStatus::Done, None);
            lane
        }

        fn sweep(
            &self,
            gh: &FakeGhCli,
            launcher: &FakeLauncher,
            mode: AutoMerge,
        ) -> Vec<AutoMergeAction> {
            let runs = self.store.overview_runs().unwrap();
            let deps = SweepDeps {
                store: &self.store,
                gh,
                repo_dir: Path::new("/repo"),
                launcher,
            };
            sweep(&deps, &runs, SCOPE, mode).unwrap()
        }

        fn details(&self, run_id: i64) -> Vec<String> {
            self.store
                .events_for_run(run_id)
                .unwrap()
                .into_iter()
                .filter(|e| e.kind == AUTO_MERGE_EVENT)
                .filter_map(|e| e.detail)
                .collect()
        }

        fn run(&self, run_id: i64) -> crate::runs::Run {
            self.store.run_by_id(run_id).unwrap().unwrap()
        }
    }

    #[test]
    fn approved_mode_starts_a_merge_for_a_ready_to_merge_pr() {
        let fx = Fixture::new();
        let lane = fx.lane("GH-1", 7);
        let gh = FakeGhCli::new()
            .with_pr_list_review_state(Ok(vec![approved(7)]))
            .with_pr_checks(7, Ok(head("abc")));
        let launcher = FakeLauncher::new();

        let actions = fx.sweep(&gh, &launcher, AutoMerge::Approved);

        let run_id = launcher.launched.borrow()[0].1;
        assert_eq!(
            actions,
            vec![AutoMergeAction::Started {
                ticket: "GH-1".to_string(),
                sha: "abc".to_string(),
                run_id,
            }]
        );
        let row = fx.run(run_id);
        assert_eq!(row.kind, AUTO_MERGE_KIND);
        assert_eq!(row.ticket, "GH-1");
        assert_eq!(row.scope, SCOPE);
        assert_eq!(row.status, RunStatus::Running);
        assert_eq!(row.pid, Some(4242));
        assert_eq!(fx.details(lane), vec![format!("started abc run {run_id}")]);
    }

    #[test]
    fn off_reads_nothing_and_starts_nothing() {
        let fx = Fixture::new();
        fx.awaiting("GH-1", 7);
        let gh = FakeGhCli::new().with_pr_list_review_state(Ok(vec![approved(7)]));
        let launcher = FakeLauncher::new();

        assert!(fx.sweep(&gh, &launcher, AutoMerge::Off).is_empty());
        assert!(gh.pr_list_review_state_calls().is_empty());
        assert!(launcher.tickets().is_empty());
    }

    #[test]
    fn approved_mode_leaves_an_unapproved_awaiting_review_pr_alone() {
        let fx = Fixture::new();
        fx.awaiting("GH-1", 7);
        let gh = FakeGhCli::new()
            .with_pr_list_review_state(Ok(vec![unapproved(7)]))
            .with_pr_checks(7, Ok(head("abc")));
        let launcher = FakeLauncher::new();

        assert!(fx.sweep(&gh, &launcher, AutoMerge::Approved).is_empty());
        assert!(launcher.tickets().is_empty());
    }

    #[test]
    fn reviewed_mode_merges_an_awaiting_review_pr() {
        let fx = Fixture::new();
        fx.awaiting("GH-1", 7);
        let gh = FakeGhCli::new()
            .with_pr_list_review_state(Ok(vec![unapproved(7)]))
            .with_pr_checks(7, Ok(head("abc")));
        let launcher = FakeLauncher::new();

        fx.sweep(&gh, &launcher, AutoMerge::Reviewed);

        assert_eq!(launcher.tickets(), vec!["GH-1"]);
    }

    #[test]
    fn reviewed_mode_leaves_an_unapproved_pr_still_under_review_alone() {
        let fx = Fixture::new();
        fx.lane("GH-1", 7); // no review watch has finished
        let gh = FakeGhCli::new()
            .with_pr_list_review_state(Ok(vec![unapproved(7)]))
            .with_pr_checks(7, Ok(head("abc")));
        let launcher = FakeLauncher::new();

        assert!(fx.sweep(&gh, &launcher, AutoMerge::Reviewed).is_empty());
    }

    #[test]
    fn starts_one_merge_per_scope_lowest_pr_first() {
        let fx = Fixture::new();
        fx.lane("GH-2", 9);
        fx.lane("GH-1", 7);
        let gh = FakeGhCli::new()
            .with_pr_list_review_state(Ok(vec![approved(9), approved(7)]))
            .with_pr_checks(7, Ok(head("abc")))
            .with_pr_checks(9, Ok(head("def")));
        let launcher = FakeLauncher::new();

        fx.sweep(&gh, &launcher, AutoMerge::Approved);
        assert_eq!(launcher.tickets(), vec!["GH-1"]);

        // The first merge is still running, so the next tick waits.
        fx.sweep(&gh, &launcher, AutoMerge::Approved);
        assert_eq!(launcher.tickets(), vec!["GH-1"]);
    }

    #[test]
    fn the_next_ticket_merges_once_the_running_merge_ends() {
        let fx = Fixture::new();
        fx.lane("GH-2", 9);
        fx.lane("GH-1", 7);
        let gh = FakeGhCli::new()
            .with_pr_list_review_state(Ok(vec![approved(9), approved(7)]))
            .with_pr_checks(7, Ok(head("abc")))
            .with_pr_checks(9, Ok(head("def")));
        let launcher = FakeLauncher::new();
        fx.sweep(&gh, &launcher, AutoMerge::Approved);
        let first = launcher.launched.borrow()[0].1;
        finish(&fx.store, first, &Ok(MergeFlowOutcome::Merged)).unwrap();
        // The merged PR drops out of the open list.
        let gh = gh.with_pr_list_review_state(Ok(vec![approved(9)]));

        fx.sweep(&gh, &launcher, AutoMerge::Approved);

        assert_eq!(launcher.tickets(), vec!["GH-1", "GH-2"]);
    }

    #[test]
    fn a_live_merge_conflict_session_holds_the_scope() {
        let fx = Fixture::new();
        fx.lane("GH-1", 7);
        fx.start("GH-5", MERGE_CONFLICT_KIND);
        let gh = FakeGhCli::new()
            .with_pr_list_review_state(Ok(vec![approved(7)]))
            .with_pr_checks(7, Ok(head("abc")));
        let launcher = FakeLauncher::new();

        assert!(fx.sweep(&gh, &launcher, AutoMerge::Approved).is_empty());
        assert!(gh.pr_list_review_state_calls().is_empty());
    }

    #[test]
    fn never_two_merges_for_the_same_head_sha() {
        let fx = Fixture::new();
        fx.lane("GH-1", 7);
        let gh = FakeGhCli::new()
            .with_pr_list_review_state(Ok(vec![approved(7)]))
            .with_pr_checks(7, Ok(head("abc")));
        let launcher = FakeLauncher::new();
        fx.sweep(&gh, &launcher, AutoMerge::Approved);
        let first = launcher.launched.borrow()[0].1;
        finish(&fx.store, first, &Err("verify_command failed".to_string())).unwrap();

        fx.sweep(&gh, &launcher, AutoMerge::Approved);
        assert_eq!(launcher.tickets(), vec!["GH-1"], "same head: no retry");
    }

    #[test]
    fn a_new_head_sha_makes_the_ticket_eligible_again() {
        let fx = Fixture::new();
        fx.lane("GH-1", 7);
        let gh = FakeGhCli::new()
            .with_pr_list_review_state(Ok(vec![approved(7)]))
            .with_pr_checks(7, Ok(head("abc")));
        let launcher = FakeLauncher::new();
        fx.sweep(&gh, &launcher, AutoMerge::Approved);
        let first = launcher.launched.borrow()[0].1;
        finish(&fx.store, first, &Err("verify_command failed".to_string())).unwrap();
        // A fix pass pushed and the PR is approved and green again.
        let fix = fx.start("GH-1", "review-fix");
        fx.end(fix, RunStatus::Done, None);
        let gh = gh.with_pr_checks(7, Ok(head("def")));

        fx.sweep(&gh, &launcher, AutoMerge::Approved);

        assert_eq!(launcher.tickets(), vec!["GH-1", "GH-1"]);
    }

    #[test]
    fn reviewed_mode_retries_an_awaiting_review_pr_on_a_new_head() {
        let fx = Fixture::new();
        fx.awaiting("GH-1", 7);
        let gh = FakeGhCli::new()
            .with_pr_list_review_state(Ok(vec![unapproved(7)]))
            .with_pr_checks(7, Ok(head("abc")));
        let launcher = FakeLauncher::new();
        fx.sweep(&gh, &launcher, AutoMerge::Reviewed);
        let first = launcher.launched.borrow()[0].1;
        // `tm merge` rebased and pushed, then ran out of checks time.
        finish(&fx.store, first, &Ok(MergeFlowOutcome::ChecksPending)).unwrap();

        fx.sweep(&gh, &launcher, AutoMerge::Reviewed);
        assert_eq!(launcher.tickets(), vec!["GH-1"], "same head: no retry");

        let gh = gh.with_pr_checks(7, Ok(head("def")));
        fx.sweep(&gh, &launcher, AutoMerge::Reviewed);
        assert_eq!(launcher.tickets(), vec!["GH-1", "GH-1"]);
    }

    #[test]
    fn a_ticket_with_a_live_run_is_skipped() {
        let fx = Fixture::new();
        fx.lane("GH-1", 7);
        fx.start("GH-1", "review-fix");
        let gh = FakeGhCli::new()
            .with_pr_list_review_state(Ok(vec![approved(7)]))
            .with_pr_checks(7, Ok(head("abc")));
        let launcher = FakeLauncher::new();

        assert!(fx.sweep(&gh, &launcher, AutoMerge::Approved).is_empty());
    }

    #[test]
    fn a_pr_missing_from_the_open_list_is_skipped() {
        let fx = Fixture::new();
        fx.lane("GH-1", 7);
        let gh = FakeGhCli::new()
            .with_pr_list_review_state(Ok(Vec::new()))
            .with_pr_checks(7, Ok(head("abc")));
        let launcher = FakeLauncher::new();

        assert!(fx.sweep(&gh, &launcher, AutoMerge::Approved).is_empty());
        assert!(gh.pr_checks_calls().is_empty());
    }

    #[test]
    fn a_gh_list_failure_is_reported_and_starts_nothing() {
        let fx = Fixture::new();
        fx.lane("GH-1", 7);
        let gh = FakeGhCli::new().with_pr_list_review_state(Err(GhError::Parse {
            command: "gh pr list".to_string(),
            message: "boom".to_string(),
        }));
        let launcher = FakeLauncher::new();

        let actions = fx.sweep(&gh, &launcher, AutoMerge::Approved);

        assert!(
            matches!(&actions[..], [AutoMergeAction::CheckFailed { ticket, .. }] if ticket.is_empty()),
            "{actions:?}"
        );
        assert!(launcher.tickets().is_empty());
    }

    #[test]
    fn a_launch_failure_fails_the_row_and_flags_the_lane() {
        let fx = Fixture::new();
        let lane = fx.lane("GH-1", 7);
        let gh = FakeGhCli::new()
            .with_pr_list_review_state(Ok(vec![approved(7)]))
            .with_pr_checks(7, Ok(head("abc")));
        let launcher = FakeLauncher::failing("no tm");

        let actions = fx.sweep(&gh, &launcher, AutoMerge::Approved);

        assert_eq!(
            actions,
            vec![AutoMergeAction::LaunchFailed {
                ticket: "GH-1".to_string(),
                sha: "abc".to_string(),
                error: "no tm".to_string(),
            }]
        );
        let run_id = launcher.launched.borrow()[0].1;
        let row = fx.run(run_id);
        assert_eq!(row.status, RunStatus::Failed);
        assert_eq!(row.blocker.as_deref(), Some("no tm"));
        assert_eq!(fx.run(lane).status, RunStatus::Review);
        assert!(
            fx.details(lane).contains(&"failed abc: no tm".to_string()),
            "{:?}",
            fx.details(lane)
        );

        // Not retried for the same head.
        fx.sweep(&gh, &launcher, AutoMerge::Approved);
        assert_eq!(launcher.tickets(), vec!["GH-1"]);
    }

    // --- finish ---

    #[test]
    fn finish_ends_a_merged_run_done_and_leaves_the_lane_alone() {
        let fx = Fixture::new();
        let lane = fx.lane("GH-1", 7);
        let run_id = fx.start("GH-1", AUTO_MERGE_KIND);

        finish(&fx.store, run_id, &Ok(MergeFlowOutcome::Merged)).unwrap();

        assert_eq!(fx.run(run_id).status, RunStatus::Done);
        assert_eq!(fx.run(lane).status, RunStatus::Done);
    }

    #[test]
    fn finish_fails_a_handed_back_or_refused_merge_and_flags_the_lane() {
        for (result, blocker) in [
            (
                Ok(MergeFlowOutcome::ConflictsHandedBack),
                "conflicts handed back",
            ),
            (Ok(MergeFlowOutcome::ChecksPending), "checks still running"),
            (
                Err("checks failed on PR #7".to_string()),
                "checks failed on PR #7",
            ),
        ] {
            let fx = Fixture::new();
            let lane = fx.lane("GH-1", 7);
            let run_id = fx.start("GH-1", AUTO_MERGE_KIND);

            finish(&fx.store, run_id, &result).unwrap();

            let row = fx.run(run_id);
            assert_eq!(row.status, RunStatus::Failed, "{blocker}");
            assert!(
                row.blocker.as_deref().is_some_and(|b| b.contains(blocker)),
                "{:?}",
                row.blocker
            );
            assert_eq!(fx.run(lane).status, RunStatus::Review, "{blocker}");
        }
    }
}
