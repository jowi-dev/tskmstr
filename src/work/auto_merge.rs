//! Merging lane pull requests without the operator: the automerge sweep
//! (GitHub issue #243) draining the merge queue (GitHub issue #247).
//!
//! The merge queue ([`crate::runs::merge_queue`]) is a per-scope, ordered
//! list of tickets waiting to merge. The operator adds tickets by hand
//! (`tm merge --queue`, the TUI), autopilot or not. Each dispatch tick runs
//! [`sweep`] after the CI-fix sweep, and it does two things:
//!
//! 1. **Enqueues.** With autopilot on and `[dispatch] auto_merge` set
//!    ([`AutoMerge`]), it appends each lane PR whose review is complete,
//!    lowest PR number first. With a merge epic set
//!    ([`RunStore::merge_epic`]), only that epic's tickets:
//!    - [`AutoMerge::Approved`] takes the overview's Ready to merge: an
//!      explicit approval, no conflicts, green checks ([`pr_signal`]).
//!    - [`AutoMerge::Reviewed`] also takes Awaiting review
//!      ([`awaiting_review`]): the bots and auto-review are done, nothing is
//!      running, and no human has looked at it.
//!
//!    It records an [`AUTO_MERGE_EVENT`] `queued <sha>` on the lane run, so
//!    a row the operator removes is not added back until the PR's head
//!    changes.
//! 2. **Drains.** It starts a detached `tm merge` for the first row, in
//!    queue order, that can merge now ([`drain_blocker`]): open PR, green
//!    checks, no requested changes, not a draft, nothing running for the
//!    ticket. A row that cannot merge yet keeps its place and records why
//!    ([`crate::runs::merge_queue::MergeQueueEntry::waiting`]). An
//!    autopilot row also still needs its `auto_merge` mode to pass, so
//!    turning autopilot off stops those but not the operator's own rows.
//!
//! **One merge per scope at a time.** Each merge rebases onto the base the
//! previous one changed, so a sweep does nothing while the scope has a live
//! [`AUTO_MERGE_KIND`] run or a live `tm merge` conflict session.
//!
//! **A failed merge holds its row.** [`finish`] removes the row when the
//! merge landed. Otherwise it holds the row with the reason, and the sweep
//! skips it until the PR's head commit changes, in either mode: the sweep
//! leaves its own rows out of [`awaiting_review`].
//!
//! Each merge gets its own [`AUTO_MERGE_KIND`] run row, registered by the
//! sweep and adopted by the detached `tm merge --auto-merge-run <ID>`,
//! which ends it with [`finish`]. The row is what makes a running or failed
//! automerge visible in `tm runs`, the overview and `tm dispatch status`,
//! and a row whose process died is reaped like any other run.
//! A merge that does not land ends the row `failed` and flags the ticket's
//! lane run for review; [`crate::work::notify::ticket_notices`] then
//! notifies once per failed row.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use crate::config::AutoMerge;
use crate::github::gh_cli::{ChecksState, GhCli, PrLifecycle, PrReviewState, ReviewDecision};
use crate::github::pr::{find_pr_for_ticket, pr_number_from_url};
use crate::overview_poll::signal::{PrSignal, pr_signal};
use crate::runs::merge_queue::{MergeQueueEntry, QueueSource};
use crate::runs::overview::{OverviewRun, awaiting_review, is_live};
use crate::runs::{FinishRun, RunEvent, RunStatus, RunStore, RunStoreError, StartRun};
use crate::work::merge::{MERGE_CONFLICT_KIND, MergeFlowOutcome};

/// `kind` (and `lane`) of an automerge's run row.
pub const AUTO_MERGE_KIND: &str = "auto-merge";

/// Event kind recorded on the ticket's lane run. Detail is
/// `queued <sha>`, `started <sha> run <id>` or `failed <sha>: <error>`.
pub const AUTO_MERGE_EVENT: &str = "auto_merge";

/// Why an autopilot row waits when the sweep runs without an `auto_merge`
/// mode: autopilot is off for the scope, or `[dispatch] auto_merge` is.
pub const AUTOPILOT_OFF: &str = "autopilot off";

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

/// Why a queued row from `source` cannot merge now, or `None` when it can.
/// `mode` is the sweep's (`Off` when autopilot is off); `awaiting` is
/// [`awaiting_review`] for the ticket's runs.
pub fn drain_blocker(
    source: QueueSource,
    mode: AutoMerge,
    state: &PrReviewState,
    awaiting: bool,
) -> Option<&'static str> {
    match state.checks {
        ChecksState::Failing => return Some("checks failing"),
        ChecksState::Pending => return Some("checks pending"),
        ChecksState::Passing | ChecksState::None => {}
    }
    if state.merge_state_status == "DRAFT" {
        return Some("draft PR");
    }
    if state.review_decision == ReviewDecision::ChangesRequested {
        return Some("changes requested");
    }
    if source == QueueSource::Autopilot {
        if mode == AutoMerge::Off {
            return Some(AUTOPILOT_OFF);
        }
        if !eligible(mode, state, awaiting) {
            return Some("review not finished");
        }
    }
    None
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
    /// The run store holding the lane runs, the queue and the automerge
    /// rows.
    pub store: &'a RunStore,
    /// Reads the PRs' review state and head commits.
    pub gh: &'a dyn GhCli,
    /// Where `gh` runs: a checkout of the scope's repo.
    pub repo_dir: &'a Path,
    /// Starts the merge.
    pub launcher: &'a dyn AutoMergeLauncher,
    /// The ticket keys under an epic, for the merge epic. Called at most
    /// once per sweep, and only when autopilot has something to enqueue.
    pub epic_tickets: &'a dyn Fn(&str) -> Result<Vec<String>, String>,
}

/// What [`sweep`] did. A sweep that changed nothing returns none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoMergeAction {
    /// Autopilot added a ticket to the queue at head commit `sha`.
    Queued {
        /// Ticket key.
        ticket: String,
        /// The PR's head commit.
        sha: String,
    },
    /// A merge was started for head commit `sha` under run row `run_id`.
    Started {
        /// Ticket key.
        ticket: String,
        /// The PR's head commit.
        sha: String,
        /// The automerge's run row.
        run_id: i64,
    },
    /// The launcher refused; the run row ended `failed`, the queue row is
    /// held, and the lane run was flagged for review.
    LaunchFailed {
        /// Ticket key.
        ticket: String,
        /// The PR's head commit.
        sha: String,
        /// The launcher's error.
        error: String,
    },
    /// A queued ticket's PR was merged or closed some other way, so its
    /// row was removed.
    Dropped {
        /// Ticket key.
        ticket: String,
        /// What happened to the PR.
        reason: String,
    },
    /// `gh` (or the tracker, for the merge epic) could not be read;
    /// retried next sweep.
    CheckFailed {
        /// The ticket whose PR could not be read, or `""` when the scope's
        /// PR list (or merge epic) could not be.
        ticket: String,
        /// The error.
        error: String,
    },
}

/// One dispatch tick's pass over `scope`'s merge queue: enqueues the
/// tickets `mode` finds [`eligible`], then starts at most one `tm merge`,
/// for the first queued row with no [`drain_blocker`] and no hold on its
/// PR's current head. `mode` is [`AutoMerge::Off`] when autopilot is off
/// for the scope; the operator's own rows still drain then.
///
/// `runs` is [`RunStore::overview_runs`] output, newest first. Nothing
/// happens while the scope has a live [`AUTO_MERGE_KIND`] or
/// [`MERGE_CONFLICT_KIND`] run.
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
    if mode == AutoMerge::Off && deps.store.merge_queue(scope)?.is_empty() {
        return Ok(Vec::new());
    }
    let scoped: Vec<&OverviewRun> = runs.iter().filter(|run| run.scope == scope).collect();
    let merging = scoped.iter().any(|run| {
        is_live(run.status) && [AUTO_MERGE_KIND, MERGE_CONFLICT_KIND].contains(&run.kind.as_str())
    });
    if merging {
        return Ok(Vec::new());
    }

    let mut by_ticket: HashMap<&str, Vec<&OverviewRun>> = HashMap::new();
    for run in scoped {
        by_ticket.entry(&run.ticket).or_default().push(run);
    }

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
    if mode != AutoMerge::Off {
        enqueue(deps, scope, mode, &by_ticket, &open, &mut actions)?;
    }
    drain(deps, scope, mode, &by_ticket, &open, &mut actions)?;
    Ok(actions)
}

/// [`awaiting_review`] over a ticket's runs minus the sweep's own
/// [`AUTO_MERGE_KIND`] rows. A failed automerge row would otherwise count as
/// a failure after the review watch and hold the ticket out of Awaiting
/// review for good. The queue's hold already keeps a failed merge from
/// retrying on the same head, so the sweep leaves its own rows out here.
fn awaiting_review_ignoring_automerge(runs: &[&OverviewRun]) -> bool {
    let reviewed: Vec<&OverviewRun> = runs
        .iter()
        .copied()
        .filter(|run| run.kind != AUTO_MERGE_KIND)
        .collect();
    awaiting_review(&reviewed)
}

/// A ticket's lane run with a PR, newest first, and that PR's number.
fn lane_pr<'r>(runs: &[&'r OverviewRun]) -> Option<(&'r OverviewRun, u64)> {
    runs.iter().find_map(|run| {
        let url = run.pr_url.as_deref().filter(|_| run.kind == "lane")?;
        Some((*run, pr_number_from_url(url)?))
    })
}

/// Appends the tickets autopilot merges on its own to the queue.
fn enqueue(
    deps: &SweepDeps<'_>,
    scope: &str,
    mode: AutoMerge,
    by_ticket: &HashMap<&str, Vec<&OverviewRun>>,
    open: &[PrReviewState],
    actions: &mut Vec<AutoMergeAction>,
) -> Result<(), RunStoreError> {
    let queued: HashSet<String> = deps
        .store
        .merge_queue(scope)?
        .into_iter()
        .map(|entry| entry.ticket)
        .collect();
    // (PR number, ticket, lane run, PR state)
    let mut candidates: Vec<(u64, &str, &OverviewRun, &PrReviewState)> = Vec::new();
    for (ticket, runs) in by_ticket {
        if queued.contains(*ticket) || runs.iter().any(|run| is_live(run.status) || run.pr_closed) {
            continue;
        }
        let Some((lane, number)) = lane_pr(runs) else {
            continue;
        };
        let Some(state) = open.iter().find(|state| state.pr.number == number) else {
            continue;
        };
        if eligible(mode, state, awaiting_review_ignoring_automerge(runs)) {
            candidates.push((number, ticket, lane, state));
        }
    }
    if candidates.is_empty() {
        return Ok(());
    }
    candidates.sort_by_key(|(number, ..)| *number);

    if let Some(epic) = deps.store.merge_epic(scope)? {
        match (deps.epic_tickets)(&epic) {
            Ok(members) => {
                let members: HashSet<String> =
                    members.iter().map(|key| key.to_uppercase()).collect();
                candidates.retain(|(_, ticket, ..)| members.contains(&ticket.to_uppercase()));
            }
            Err(error) => {
                actions.push(AutoMergeAction::CheckFailed {
                    ticket: String::new(),
                    error: format!("merge epic {epic}: {error}"),
                });
                return Ok(());
            }
        }
    }

    for (number, ticket, lane, _) in candidates {
        let sha = match head_sha(deps, ticket, number, actions) {
            Some(Ok(sha)) => sha,
            _ => continue,
        };
        if handled(&deps.store.events_for_run(lane.id)?, &sha) {
            continue;
        }
        deps.store
            .enqueue_merge(scope, ticket, QueueSource::Autopilot)?;
        deps.store
            .add_event(lane.id, AUTO_MERGE_EVENT, Some(&format!("queued {sha}")))?;
        actions.push(AutoMergeAction::Queued {
            ticket: ticket.to_string(),
            sha,
        });
    }
    Ok(())
}

/// PR `number`'s head commit: `Some(Ok(sha))` while it is open,
/// `Some(Err(lifecycle))` once it is merged or closed, and `None` when `gh`
/// failed (recorded as a [`AutoMergeAction::CheckFailed`]) or reported no
/// head.
fn head_sha(
    deps: &SweepDeps<'_>,
    ticket: &str,
    number: u64,
    actions: &mut Vec<AutoMergeAction>,
) -> Option<Result<String, PrLifecycle>> {
    match deps.gh.pr_checks(deps.repo_dir, number) {
        // An empty SHA would make every hold and event look stale.
        Ok(checks) if checks.lifecycle == PrLifecycle::Open => {
            Some(Ok(checks.head_sha)).filter(|sha| sha.as_ref().is_ok_and(|s| !s.is_empty()))
        }
        Ok(checks) => Some(Err(checks.lifecycle)),
        Err(err) => {
            actions.push(AutoMergeAction::CheckFailed {
                ticket: ticket.to_string(),
                error: err.to_string(),
            });
            None
        }
    }
}

/// Starts the merge for the first queued row that can merge now, and
/// records why each row before it could not.
fn drain(
    deps: &SweepDeps<'_>,
    scope: &str,
    mode: AutoMerge,
    by_ticket: &HashMap<&str, Vec<&OverviewRun>>,
    open: &[PrReviewState],
    actions: &mut Vec<AutoMergeAction>,
) -> Result<(), RunStoreError> {
    let prs: Vec<_> = open.iter().map(|state| state.pr.clone()).collect();
    for entry in deps.store.merge_queue(scope)? {
        let ticket = entry.ticket.as_str();
        let runs = by_ticket.get(ticket).map(Vec::as_slice).unwrap_or_default();
        let wait = |reason: &str| deps.store.set_merge_waiting(scope, ticket, Some(reason));
        if runs.iter().any(|run| run.pr_closed) {
            drop_row(deps, scope, ticket, "PR closed", actions)?;
            continue;
        }
        if runs.iter().any(|run| is_live(run.status)) {
            wait("a run is active")?;
            continue;
        }
        let lane = lane_pr(runs);
        let number = match lane {
            Some((_, number)) => Some(number),
            None => find_pr_for_ticket(&prs, ticket, &|_| true).map(|pr| pr.number),
        };
        let Some(number) = number else {
            wait("no open PR")?;
            continue;
        };
        let state = open.iter().find(|state| state.pr.number == number);
        if let Some(state) = state
            && let Some(reason) = drain_blocker(
                entry.source,
                mode,
                state,
                awaiting_review_ignoring_automerge(runs),
            )
        {
            wait(reason)?;
            continue;
        }
        let sha = match head_sha(deps, ticket, number, actions) {
            Some(Ok(sha)) => sha,
            Some(Err(lifecycle)) => {
                let reason = match lifecycle {
                    PrLifecycle::Merged => "PR merged",
                    _ => "PR closed",
                };
                drop_row(deps, scope, ticket, reason, actions)?;
                continue;
            }
            None => continue,
        };
        if state.is_none() {
            // Open, yet missing from `gh pr list`: wait for it to show.
            wait("PR not listed yet")?;
            continue;
        }
        if entry.held_at(&sha) {
            continue;
        }
        actions.push(start(deps, scope, &entry, lane.map(|(run, _)| run), sha)?);
        break;
    }
    Ok(())
}

fn drop_row(
    deps: &SweepDeps<'_>,
    scope: &str,
    ticket: &str,
    reason: &str,
    actions: &mut Vec<AutoMergeAction>,
) -> Result<(), RunStoreError> {
    deps.store.dequeue_merge(scope, ticket)?;
    actions.push(AutoMergeAction::Dropped {
        ticket: ticket.to_string(),
        reason: reason.to_string(),
    });
    Ok(())
}

/// Registers the automerge's run row, records the head on the queue row
/// and (when there is one) the lane run, and launches the merge.
fn start(
    deps: &SweepDeps<'_>,
    scope: &str,
    entry: &MergeQueueEntry,
    lane: Option<&OverviewRun>,
    sha: String,
) -> Result<AutoMergeAction, RunStoreError> {
    let ticket = entry.ticket.as_str();
    let run_id = deps.store.start_run(&StartRun {
        ticket: ticket.to_string(),
        scope: scope.to_string(),
        lane: AUTO_MERGE_KIND.to_string(),
        worktree: lane.map_or_else(
            || deps.repo_dir.display().to_string(),
            |lane| lane.worktree.clone(),
        ),
        branch: None,
        pid: None,
        kind: AUTO_MERGE_KIND.to_string(),
        log_path: None,
    })?;
    // Recorded before the launch, so whatever becomes of the process a
    // failure holds the row at this head.
    deps.store.mark_merge_started(scope, ticket, &sha)?;
    if let Some(lane) = lane {
        deps.store.add_event(
            lane.id,
            AUTO_MERGE_EVENT,
            Some(&format!("started {sha} run {run_id}")),
        )?;
    }
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
            if let Some(lane) = lane {
                deps.store.add_event(
                    lane.id,
                    AUTO_MERGE_EVENT,
                    Some(&format!("failed {sha}: {error}")),
                )?;
            }
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
/// `done` when it merged, which also removes the ticket from the merge
/// queue. Otherwise `failed` with the reason as its blocker, the queue row
/// held with that reason, and the ticket's newest lane run flagged for
/// review.
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
    let Some(run) = store.run_by_id(run_id)? else {
        return Ok(());
    };
    let Some(blocker) = blocker else {
        store.dequeue_merge(&run.scope, &run.ticket)?;
        return Ok(());
    };
    store.hold_merge(&run.scope, &run.ticket, &blocker)?;
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
    use crate::runs::merge_queue::{MergeQueueEntry, QueueSource};
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
            self.sweep_with_epics(gh, launcher, mode, &|epic| {
                Err(format!("no children for {epic}"))
            })
        }

        fn sweep_with_epics(
            &self,
            gh: &FakeGhCli,
            launcher: &FakeLauncher,
            mode: AutoMerge,
            epic_tickets: &dyn Fn(&str) -> Result<Vec<String>, String>,
        ) -> Vec<AutoMergeAction> {
            let runs = self.store.overview_runs().unwrap();
            let deps = SweepDeps {
                store: &self.store,
                gh,
                repo_dir: Path::new("/repo"),
                launcher,
                epic_tickets,
            };
            sweep(&deps, &runs, SCOPE, mode).unwrap()
        }

        fn queue(&self) -> Vec<MergeQueueEntry> {
            self.store.merge_queue(SCOPE).unwrap()
        }

        fn queued(&self) -> Vec<String> {
            self.queue().into_iter().map(|e| e.ticket).collect()
        }

        fn enqueue(&self, ticket: &str) {
            self.store
                .enqueue_merge(SCOPE, ticket, QueueSource::Manual)
                .unwrap();
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
            vec![
                AutoMergeAction::Queued {
                    ticket: "GH-1".to_string(),
                    sha: "abc".to_string(),
                },
                AutoMergeAction::Started {
                    ticket: "GH-1".to_string(),
                    sha: "abc".to_string(),
                    run_id,
                }
            ]
        );
        let row = fx.run(run_id);
        assert_eq!(row.kind, AUTO_MERGE_KIND);
        assert_eq!(row.ticket, "GH-1");
        assert_eq!(row.scope, SCOPE);
        assert_eq!(row.status, RunStatus::Running);
        assert_eq!(row.pid, Some(4242));
        assert_eq!(
            fx.details(lane),
            vec![
                "queued abc".to_string(),
                format!("started abc run {run_id}")
            ]
        );
        let queue = fx.queue();
        assert_eq!(queue[0].source, QueueSource::Autopilot);
        assert_eq!(queue[0].head_sha.as_deref(), Some("abc"));
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
            actions.last(),
            Some(&AutoMergeAction::LaunchFailed {
                ticket: "GH-1".to_string(),
                sha: "abc".to_string(),
                error: "no tm".to_string(),
            })
        );
        assert_eq!(fx.queue()[0].held.as_deref(), Some("no tm"));
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

    // --- the merge queue (GitHub issue #247) ---

    fn titled(state: PrReviewState, title: &str) -> PrReviewState {
        PrReviewState {
            pr: PrInfo {
                title: title.to_string(),
                ..state.pr
            },
            ..state
        }
    }

    fn started(actions: &[AutoMergeAction]) -> Vec<String> {
        actions
            .iter()
            .filter_map(|a| match a {
                AutoMergeAction::Started { ticket, .. } => Some(ticket.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_manual_row_merges_with_autopilot_off_once_checks_are_green() {
        let fx = Fixture::new();
        fx.lane("GH-1", 7);
        fx.enqueue("GH-1");
        let pending = review_state(7, ReviewDecision::ReviewRequired, ChecksState::Pending);
        let gh = FakeGhCli::new()
            .with_pr_list_review_state(Ok(vec![pending]))
            .with_pr_checks(7, Ok(head("abc")));
        let launcher = FakeLauncher::new();

        assert!(started(&fx.sweep(&gh, &launcher, AutoMerge::Off)).is_empty());
        assert_eq!(fx.queue()[0].waiting.as_deref(), Some("checks pending"));

        let gh = gh.with_pr_list_review_state(Ok(vec![unapproved(7)]));
        let actions = fx.sweep(&gh, &launcher, AutoMerge::Off);

        assert_eq!(started(&actions), vec!["GH-1"]);
        assert_eq!(fx.queue()[0].waiting, None);
        assert_eq!(fx.queue()[0].head_sha.as_deref(), Some("abc"));
    }

    #[test]
    fn with_autopilot_off_an_autopilot_row_waits() {
        let fx = Fixture::new();
        fx.lane("GH-1", 7);
        fx.store
            .enqueue_merge(SCOPE, "GH-1", QueueSource::Autopilot)
            .unwrap();
        let gh = FakeGhCli::new()
            .with_pr_list_review_state(Ok(vec![approved(7)]))
            .with_pr_checks(7, Ok(head("abc")));
        let launcher = FakeLauncher::new();

        assert!(started(&fx.sweep(&gh, &launcher, AutoMerge::Off)).is_empty());
        assert_eq!(fx.queue()[0].waiting.as_deref(), Some(AUTOPILOT_OFF));
    }

    #[test]
    fn the_highest_eligible_row_merges_first_and_others_keep_their_place() {
        let fx = Fixture::new();
        fx.lane("GH-1", 7);
        fx.lane("GH-2", 9);
        fx.lane("GH-3", 11);
        fx.enqueue("GH-3");
        fx.enqueue("GH-2");
        fx.enqueue("GH-1");
        let red = review_state(11, ReviewDecision::ReviewRequired, ChecksState::Failing);
        let gh = FakeGhCli::new()
            .with_pr_list_review_state(Ok(vec![approved(7), approved(9), red]))
            .with_pr_checks(7, Ok(head("a7")))
            .with_pr_checks(9, Ok(head("a9")));
        let launcher = FakeLauncher::new();

        let actions = fx.sweep(&gh, &launcher, AutoMerge::Off);

        // GH-3 is first but red; GH-2 is the highest eligible row.
        assert_eq!(started(&actions), vec!["GH-2"]);
        assert_eq!(fx.queued(), vec!["GH-3", "GH-2", "GH-1"]);
        assert_eq!(fx.queue()[0].waiting.as_deref(), Some("checks failing"));
    }

    #[test]
    fn reordering_changes_which_ticket_merges_next() {
        let fx = Fixture::new();
        fx.lane("GH-1", 7);
        fx.lane("GH-2", 9);
        fx.enqueue("GH-1");
        fx.enqueue("GH-2");
        fx.store.move_merge(SCOPE, "GH-2", 0).unwrap();
        let gh = FakeGhCli::new()
            .with_pr_list_review_state(Ok(vec![approved(7), approved(9)]))
            .with_pr_checks(7, Ok(head("a7")))
            .with_pr_checks(9, Ok(head("a9")));
        let launcher = FakeLauncher::new();

        let actions = fx.sweep(&gh, &launcher, AutoMerge::Off);

        assert_eq!(started(&actions), vec!["GH-2"]);
    }

    #[test]
    fn a_held_row_is_skipped_until_its_head_changes() {
        let fx = Fixture::new();
        fx.lane("GH-1", 7);
        fx.lane("GH-2", 9);
        fx.enqueue("GH-1");
        fx.enqueue("GH-2");
        let gh = FakeGhCli::new()
            .with_pr_list_review_state(Ok(vec![approved(7), approved(9)]))
            .with_pr_checks(7, Ok(head("a7")))
            .with_pr_checks(9, Ok(head("a9")));
        let launcher = FakeLauncher::new();
        fx.sweep(&gh, &launcher, AutoMerge::Off);
        let first = launcher.launched.borrow()[0].1;
        finish(&fx.store, first, &Ok(MergeFlowOutcome::ConflictsHandedBack)).unwrap();
        assert!(fx.queue()[0].held.is_some());

        // Held at a7: GH-2 merges instead, and GH-1 keeps first place.
        let actions = fx.sweep(&gh, &launcher, AutoMerge::Off);
        assert_eq!(started(&actions), vec!["GH-2"]);
        assert_eq!(fx.queued(), vec!["GH-1", "GH-2"]);

        // A new head on GH-1 releases the hold.
        let second = launcher.launched.borrow()[1].1;
        finish(&fx.store, second, &Ok(MergeFlowOutcome::Merged)).unwrap();
        let gh = gh
            .with_pr_list_review_state(Ok(vec![approved(7)]))
            .with_pr_checks(7, Ok(head("b7")));
        let actions = fx.sweep(&gh, &launcher, AutoMerge::Off);
        assert_eq!(started(&actions), vec!["GH-1"]);
        assert_eq!(fx.queue()[0].held, None);
    }

    #[test]
    fn a_row_whose_pr_closed_is_dropped() {
        let fx = Fixture::new();
        fx.lane("GH-1", 7);
        fx.enqueue("GH-1");
        let gh = FakeGhCli::new()
            .with_pr_list_review_state(Ok(Vec::new()))
            .with_pr_checks(
                7,
                Ok(PrChecks {
                    lifecycle: PrLifecycle::Merged,
                    ..head("abc")
                }),
            );
        let launcher = FakeLauncher::new();

        let actions = fx.sweep(&gh, &launcher, AutoMerge::Off);

        assert!(
            matches!(&actions[..], [AutoMergeAction::Dropped { ticket, .. }] if ticket == "GH-1"),
            "{actions:?}"
        );
        assert!(fx.queued().is_empty());
    }

    #[test]
    fn a_manual_row_without_a_lane_run_finds_its_pr_by_key() {
        let fx = Fixture::new();
        fx.enqueue("GH-3");
        let gh = FakeGhCli::new()
            .with_pr_list_review_state(Ok(vec![titled(unapproved(12), "[GH-3] fix")]))
            .with_pr_checks(12, Ok(head("abc")));
        let launcher = FakeLauncher::new();

        let actions = fx.sweep(&gh, &launcher, AutoMerge::Off);

        assert_eq!(started(&actions), vec!["GH-3"]);
    }

    #[test]
    fn a_manual_row_with_no_pr_waits() {
        let fx = Fixture::new();
        fx.enqueue("GH-3");
        let gh = FakeGhCli::new().with_pr_list_review_state(Ok(Vec::new()));
        let launcher = FakeLauncher::new();

        assert!(fx.sweep(&gh, &launcher, AutoMerge::Off).is_empty());
        assert_eq!(fx.queue()[0].waiting.as_deref(), Some("no open PR"));
    }

    #[test]
    fn a_removed_autopilot_row_is_not_requeued_for_the_same_head() {
        let fx = Fixture::new();
        fx.lane("GH-1", 7);
        let gh = FakeGhCli::new()
            .with_pr_list_review_state(Ok(vec![approved(7)]))
            .with_pr_checks(7, Ok(head("abc")));
        let launcher = FakeLauncher::new();
        fx.sweep(&gh, &launcher, AutoMerge::Approved);
        let first = launcher.launched.borrow()[0].1;
        finish(&fx.store, first, &Err("refused".to_string())).unwrap();
        fx.store.dequeue_merge(SCOPE, "GH-1").unwrap();

        fx.sweep(&gh, &launcher, AutoMerge::Approved);
        assert!(fx.queued().is_empty(), "removal sticks for head abc");

        let gh = gh.with_pr_checks(7, Ok(head("def")));
        fx.sweep(&gh, &launcher, AutoMerge::Approved);
        assert_eq!(fx.queued(), vec!["GH-1"], "a new head is queued again");
    }

    #[test]
    fn a_merge_epic_limits_what_autopilot_queues_but_not_manual_rows() {
        let fx = Fixture::new();
        fx.lane("GH-1", 7);
        fx.lane("GH-2", 9);
        fx.lane("GH-3", 11);
        fx.enqueue("GH-3");
        fx.store.set_merge_epic(SCOPE, Some("GH-100")).unwrap();
        let gh = FakeGhCli::new()
            .with_pr_list_review_state(Ok(vec![approved(7), approved(9), approved(11)]))
            .with_pr_checks(7, Ok(head("a7")))
            .with_pr_checks(9, Ok(head("a9")))
            .with_pr_checks(11, Ok(head("a11")));
        let launcher = FakeLauncher::new();
        let epic = |epic: &str| {
            assert_eq!(epic, "GH-100");
            Ok(vec!["GH-2".to_string()])
        };

        let actions = fx.sweep_with_epics(&gh, &launcher, AutoMerge::Approved, &epic);

        assert_eq!(fx.queued(), vec!["GH-3", "GH-2"]);
        assert_eq!(started(&actions), vec!["GH-3"]);
    }

    #[test]
    fn an_unreadable_merge_epic_queues_nothing_on_its_own() {
        let fx = Fixture::new();
        fx.lane("GH-1", 7);
        fx.store.set_merge_epic(SCOPE, Some("GH-100")).unwrap();
        let gh = FakeGhCli::new()
            .with_pr_list_review_state(Ok(vec![approved(7)]))
            .with_pr_checks(7, Ok(head("a7")));
        let launcher = FakeLauncher::new();

        let actions = fx.sweep(&gh, &launcher, AutoMerge::Approved);

        assert!(fx.queued().is_empty());
        assert!(
            matches!(&actions[..], [AutoMergeAction::CheckFailed { .. }]),
            "{actions:?}"
        );
    }

    // --- finish ---

    #[test]
    fn finish_ends_a_merged_run_done_and_leaves_the_lane_alone() {
        let fx = Fixture::new();
        let lane = fx.lane("GH-1", 7);
        let run_id = fx.start("GH-1", AUTO_MERGE_KIND);

        fx.enqueue("GH-1");
        fx.enqueue("GH-2");

        finish(&fx.store, run_id, &Ok(MergeFlowOutcome::Merged)).unwrap();

        assert_eq!(fx.run(run_id).status, RunStatus::Done);
        assert_eq!(fx.run(lane).status, RunStatus::Done);
        assert_eq!(fx.queued(), vec!["GH-2"]);
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
            fx.enqueue("GH-1");
            let run_id = fx.start("GH-1", AUTO_MERGE_KIND);

            finish(&fx.store, run_id, &result).unwrap();

            let held = fx.queue()[0].held.clone();
            assert!(held.is_some_and(|h| h.contains(blocker)), "{blocker}");
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
