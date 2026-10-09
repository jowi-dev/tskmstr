//! The cross-project overview's data model (GitHub issue #82, slice 1 of
//! ADR-0009 `docs/decisions/0009-cross-project-overview.md`).
//!
//! `tm overview` is per **ticket**, not per run: every run recorded for a
//! `(scope, ticket)` collapses into one [`OverviewRow`], placed in the
//! highest-ranked [`Stage`] its signals reach (ADR-0009 decision 1). This
//! module is the pure part of that: stage derivation ([`derive_stage`]),
//! the join over run rows ([`join_rows`]), attention-queue ordering
//! ([`attention_order`]), and repo-root resolution ([`resolve_repo_root`]).
//! It does no I/O; the run rows come from [`super::RunStore::overview_runs`],
//! tracker/PR signals from a per-repo poller (a later slice), and the
//! worktree fallback for [`RepoRoot`] from a caller-supplied resolver.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::RunStatus;

/// One run row as the overview reads it, from
/// [`super::RunStore::overview_runs`]. A slimmer projection than
/// [`super::Run`] plus the two derived fields watch also computes
/// (`awaiting_input`, `heartbeat_age_secs`) and the age the queue's tie
/// break needs.
#[derive(Debug, Clone, PartialEq)]
pub struct OverviewRun {
    /// Row id.
    pub id: i64,
    /// See [`super::Run::scope`].
    pub scope: String,
    /// Ticket key.
    pub ticket: String,
    /// See [`super::Run::kind`].
    pub kind: String,
    /// Current status.
    pub status: RunStatus,
    /// See [`super::is_awaiting_input`].
    pub awaiting_input: bool,
    /// Seconds since the last heartbeat (or `started_at` if none); `None`
    /// once the run has ended.
    pub heartbeat_age_secs: Option<i64>,
    /// Seconds the run has been in its current state: since `ended_at` for
    /// an ended run, since the latest event for one awaiting input, since
    /// `started_at` otherwise.
    pub state_age_secs: i64,
    /// See [`super::Run::worktree`].
    pub worktree: String,
    /// See [`super::Run::pr_url`].
    pub pr_url: Option<String>,
    /// See [`super::Run::repo_root`].
    pub repo_root: Option<String>,
}

/// A ticket's identity across projects: GitHub issue numbers restart per
/// repo, so a bare key is only unique within its scope (issue #10).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TicketKey {
    /// See [`super::Run::scope`].
    pub scope: String,
    /// Ticket key, e.g. `GH-82`.
    pub ticket: String,
}

impl TicketKey {
    /// Builds a key from its parts.
    pub fn new(scope: impl Into<String>, ticket: impl Into<String>) -> Self {
        Self {
            scope: scope.into(),
            ticket: ticket.into(),
        }
    }
}

/// Where a ticket is in its lifecycle, in attention-queue order: variants
/// are declared highest-ranked first, so the derived `Ord` *is* the queue
/// order (ADR-0009 decision 1's table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Stage {
    /// A live session is waiting on the operator.
    NeedsInput,
    /// PR open, approved/mergeable, checks green.
    ReadyToMerge,
    /// PR open with no approving review yet.
    NeedsReview,
    /// PR open but conflicted or with failing checks.
    Conflicted,
    /// Drifted (#80), a failed/interrupted/blocked latest run, or a running
    /// run whose heartbeat has gone stale.
    Stuck,
    /// A live run that needs nothing from the operator.
    Running,
    /// In the operator's ready list with no run recorded.
    NotStarted,
}

impl Stage {
    /// The stage's 1-based rank in ADR-0009's table (1 = most urgent).
    pub fn rank(self) -> u8 {
        self as u8 + 1
    }
}

/// A ticket's PR state, as the per-repo tracker poller classifies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrState {
    /// Approved/mergeable with green checks.
    ReadyToMerge,
    /// No approving review yet.
    NeedsReview,
    /// Merge state conflicted, or checks failing.
    Conflicted,
}

/// Everything [`derive_stage`] reads for one ticket.
#[derive(Debug, Clone, Default)]
pub struct StageSignals {
    /// Status of the run standing for the ticket, `None` with no run.
    pub run_status: Option<RunStatus>,
    /// See [`OverviewRun::awaiting_input`].
    pub awaiting_input: bool,
    /// The run is `running` but its heartbeat is older than the staleness
    /// threshold.
    pub heartbeat_stale: bool,
    /// The ticket's PR state, if a tracker snapshot has one.
    pub pr: Option<PrState>,
    /// Whether #80's drift audit flagged the ticket.
    pub drifted: bool,
    /// Whether the ticket is in the operator's ready list.
    pub in_ready_list: bool,
}

/// The highest-ranked [`Stage`] `signals` reach, or `None` when the ticket
/// needs no row (e.g. its latest run is done and nothing else is known
/// about it).
///
/// Beyond ADR-0009's table, a few run statuses need a home: a
/// `hibernated` run (#64) is an interactive session that went idle waiting
/// on the operator, so it reads as [`Stage::NeedsInput`]; a `blocked` run
/// escalated and reads as [`Stage::Stuck`]; a `review` run reads as
/// [`Stage::NeedsReview`] until a PR state says otherwise; a `queued` run
/// reads as [`Stage::Running`]. Awaiting input outranks a stale heartbeat,
/// since a session waiting on the operator emits no heartbeats.
pub fn derive_stage(signals: &StageSignals) -> Option<Stage> {
    use RunStatus::*;

    let status = signals.run_status;
    if (status == Some(Running) && signals.awaiting_input) || status == Some(Hibernated) {
        return Some(Stage::NeedsInput);
    }
    match signals.pr {
        Some(PrState::ReadyToMerge) => return Some(Stage::ReadyToMerge),
        Some(PrState::NeedsReview) => return Some(Stage::NeedsReview),
        Some(PrState::Conflicted) => return Some(Stage::Conflicted),
        None => {}
    }
    match status {
        Some(Review) => Some(Stage::NeedsReview),
        _ if signals.drifted => Some(Stage::Stuck),
        Some(Failed | Interrupted | Blocked) => Some(Stage::Stuck),
        Some(Running) if signals.heartbeat_stale => Some(Stage::Stuck),
        Some(Running | Queued) => Some(Stage::Running),
        None if signals.in_ready_list => Some(Stage::NotStarted),
        _ => None,
    }
}

/// Where a row's actions run: the main checkout of the row's repo
/// (ADR-0009 decision 5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoRoot {
    /// Recorded on a run row (`runs.repo_root`), or known from the polled
    /// repo for a [`Stage::NotStarted`] row.
    Recorded(PathBuf),
    /// Resolved from a run's still-existing worktree.
    FromWorktree(PathBuf),
    /// Resolved neither way: the row stays visible with its actions
    /// disabled.
    Unresolved,
}

impl RepoRoot {
    /// The resolved path, if any.
    pub fn path(&self) -> Option<&Path> {
        match self {
            RepoRoot::Recorded(path) | RepoRoot::FromWorktree(path) => Some(path),
            RepoRoot::Unresolved => None,
        }
    }

    /// Whether the row's actions can run (a repo root is known).
    pub fn actions_enabled(&self) -> bool {
        self.path().is_some()
    }
}

/// Resolves a row's [`RepoRoot`]: `recorded` wins; otherwise
/// `from_worktree` (in production, [`crate::work::git::GitOps::repo_root`]
/// over `git rev-parse --git-common-dir`) is asked about `worktree`, unless
/// `worktree` is empty.
pub fn resolve_repo_root(
    recorded: Option<&str>,
    worktree: &str,
    from_worktree: &dyn Fn(&Path) -> Option<PathBuf>,
) -> RepoRoot {
    if let Some(recorded) = recorded {
        return RepoRoot::Recorded(PathBuf::from(recorded));
    }
    if worktree.is_empty() {
        return RepoRoot::Unresolved;
    }
    from_worktree(Path::new(worktree)).map_or(RepoRoot::Unresolved, RepoRoot::FromWorktree)
}

/// A ticket from the operator's ready list (the `tm ready` query, per
/// polled scope), with the repo it was polled from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyTicket {
    /// The ticket.
    pub key: TicketKey,
    /// Main checkout of the repo whose tracker listed it.
    pub repo_root: PathBuf,
}

/// The tracker-side signals [`join_rows`] layers over the run rows. Empty
/// (the default) until the per-repo poller exists, in which case the
/// overview shows runs-only stages.
#[derive(Debug, Clone, Default)]
pub struct TrackerSignals {
    /// PR state per ticket.
    pub pr: HashMap<TicketKey, PrState>,
    /// Tickets #80's drift audit flagged.
    pub drifted: HashSet<TicketKey>,
    /// The operator's ready list across polled scopes.
    pub ready: Vec<ReadyTicket>,
}

/// One ticket's row in the overview.
#[derive(Debug, Clone, PartialEq)]
pub struct OverviewRow {
    /// The ticket.
    pub key: TicketKey,
    /// Its highest-ranked stage.
    pub stage: Stage,
    /// Seconds in that stage, for the queue's tie break; `0` when unknown
    /// (a [`Stage::NotStarted`] row has no run to age from).
    pub stage_age_secs: i64,
    /// The run the stage was derived from, `None` for a not-started row.
    pub run_id: Option<i64>,
    /// That run's status.
    pub run_status: Option<RunStatus>,
    /// That run's kind.
    pub kind: Option<String>,
    /// The newest PR URL any of the ticket's runs recorded.
    pub pr_url: Option<String>,
    /// That run's worktree.
    pub worktree: Option<String>,
    /// Where the row's actions run.
    pub repo_root: RepoRoot,
}

/// Joins `runs` (newest first, as [`super::RunStore::overview_runs`]
/// returns them) and `tracker` into one [`OverviewRow`] per
/// `(scope, ticket)`, in [`attention_order`].
///
/// Which of a ticket's runs stand for it: its newest run, plus any still
/// live one (`running`, `queued`, `hibernated`) — a newer audit or fix run
/// must not hide a lane still waiting on the operator, while an older
/// failed attempt superseded by a newer run is history, not a signal. Of
/// those, the one reaching the highest stage wins (newest on a tie). A
/// `running` run is stale once its heartbeat is older than
/// `stale_after_secs`.
///
/// The row's [`RepoRoot`] is the newest `repo_root` any of the ticket's
/// runs recorded, else the winning run's worktree resolved through
/// `from_worktree`. Ready-list tickets with no run at all become
/// [`Stage::NotStarted`] rows rooted at the repo they were polled from.
pub fn join_rows(
    runs: &[OverviewRun],
    tracker: &TrackerSignals,
    stale_after_secs: i64,
    from_worktree: &dyn Fn(&Path) -> Option<PathBuf>,
) -> Vec<OverviewRow> {
    let mut by_ticket: BTreeMap<TicketKey, Vec<&OverviewRun>> = BTreeMap::new();
    for run in runs {
        by_ticket
            .entry(TicketKey::new(&run.scope, &run.ticket))
            .or_default()
            .push(run);
    }

    let mut rows: Vec<OverviewRow> = by_ticket
        .iter()
        .filter_map(|(key, runs)| ticket_row(key, runs, tracker, stale_after_secs, from_worktree))
        .collect();

    rows.extend(
        tracker
            .ready
            .iter()
            .filter(|ready| !by_ticket.contains_key(&ready.key))
            .map(|ready| OverviewRow {
                key: ready.key.clone(),
                stage: Stage::NotStarted,
                stage_age_secs: 0,
                run_id: None,
                run_status: None,
                kind: None,
                pr_url: None,
                worktree: None,
                repo_root: RepoRoot::Recorded(ready.repo_root.clone()),
            }),
    );

    attention_order(&mut rows);
    rows
}

/// [`join_rows`] for one ticket with at least one run (`runs` newest
/// first), or `None` when no standing run reaches a stage.
fn ticket_row(
    key: &TicketKey,
    runs: &[&OverviewRun],
    tracker: &TrackerSignals,
    stale_after_secs: i64,
    from_worktree: &dyn Fn(&Path) -> Option<PathBuf>,
) -> Option<OverviewRow> {
    let standing = runs
        .iter()
        .enumerate()
        .filter(|(i, run)| *i == 0 || is_live(run.status))
        .map(|(_, run)| *run);

    let (stage, run) = standing
        .filter_map(|run| {
            let signals = StageSignals {
                run_status: Some(run.status),
                awaiting_input: run.awaiting_input,
                heartbeat_stale: run.status == RunStatus::Running
                    && run
                        .heartbeat_age_secs
                        .is_some_and(|age| age > stale_after_secs),
                pr: tracker.pr.get(key).copied(),
                drifted: tracker.drifted.contains(key),
                in_ready_list: false,
            };
            derive_stage(&signals).map(|stage| (stage, run))
        })
        // `min_by_key` keeps the first of equal minima, i.e. the newest run.
        .min_by_key(|(stage, _)| *stage)?;

    let recorded_root = runs.iter().find_map(|run| run.repo_root.as_deref());
    Some(OverviewRow {
        key: key.clone(),
        stage,
        stage_age_secs: run.state_age_secs,
        run_id: Some(run.id),
        run_status: Some(run.status),
        kind: Some(run.kind.clone()),
        pr_url: runs.iter().find_map(|run| run.pr_url.clone()),
        worktree: Some(run.worktree.clone()),
        repo_root: resolve_repo_root(recorded_root, &run.worktree, from_worktree),
    })
}

/// How recently a scope must have had a run end to stay in the overview
/// with nothing live: 7 days (ADR-0009 decision 3).
pub const ACTIVE_SCOPE_WINDOW_SECS: i64 = 7 * 24 * 60 * 60;

/// The runs of every scope still "in flight" (ADR-0009 decision 3): one with
/// a live run (`running`, `queued`, `hibernated`) or a run that ended within
/// `window_secs`. A repo the operator stopped working in drops out on its
/// own, taking its weeks-old failed runs with it, rather than parking them
/// in [`Stage::Stuck`] forever. Order is preserved.
pub fn active_scope_runs(runs: &[OverviewRun], window_secs: i64) -> Vec<OverviewRun> {
    let active: HashSet<&str> = runs
        .iter()
        .filter(|run| is_live(run.status) || run.state_age_secs <= window_secs)
        .map(|run| run.scope.as_str())
        .collect();
    runs.iter()
        .filter(|run| active.contains(run.scope.as_str()))
        .cloned()
        .collect()
}

/// Whether a run with `status` is still live: it can still change on its
/// own or is waiting on the operator to resume it.
fn is_live(status: RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Running | RunStatus::Queued | RunStatus::Hibernated
    )
}

/// Sorts `rows` into attention-queue order: by [`Stage`] rank, then oldest
/// in its stage first, then by key so the order is stable across refreshes.
pub fn attention_order(rows: &mut [OverviewRow]) {
    rows.sort_by(|a, b| {
        a.stage
            .cmp(&b.stage)
            .then(b.stage_age_secs.cmp(&a.stage_age_secs))
            .then_with(|| a.key.cmp(&b.key))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY_SECS: i64 = 24 * 60 * 60;

    fn no_worktree(_: &Path) -> Option<PathBuf> {
        None
    }

    fn signals(status: RunStatus) -> StageSignals {
        StageSignals {
            run_status: Some(status),
            ..StageSignals::default()
        }
    }

    fn run(id: i64, ticket: &str, status: RunStatus) -> OverviewRun {
        OverviewRun {
            id,
            scope: "gh:me/proj".to_string(),
            ticket: ticket.to_string(),
            kind: "lane".to_string(),
            status,
            awaiting_input: false,
            heartbeat_age_secs: (!status.is_terminal()).then_some(5),
            state_age_secs: 100,
            worktree: format!("/wt/{}", ticket.to_lowercase()),
            pr_url: None,
            repo_root: None,
        }
    }

    fn key(ticket: &str) -> TicketKey {
        TicketKey::new("gh:me/proj", ticket)
    }

    // --- derive_stage: ADR-0009 decision 1's table ---

    #[test]
    fn ranks_follow_the_adr_table() {
        let ranks: Vec<u8> = [
            Stage::NeedsInput,
            Stage::ReadyToMerge,
            Stage::NeedsReview,
            Stage::Conflicted,
            Stage::Stuck,
            Stage::Running,
            Stage::NotStarted,
        ]
        .iter()
        .map(|s| s.rank())
        .collect();
        assert_eq!(ranks, vec![1, 2, 3, 4, 5, 6, 7]);
        assert!(Stage::NeedsInput < Stage::NotStarted);
    }

    #[test]
    fn a_running_run_awaiting_input_needs_input() {
        let s = StageSignals {
            awaiting_input: true,
            ..signals(RunStatus::Running)
        };
        assert_eq!(derive_stage(&s), Some(Stage::NeedsInput));
    }

    #[test]
    fn awaiting_input_outranks_a_stale_heartbeat() {
        let s = StageSignals {
            awaiting_input: true,
            heartbeat_stale: true,
            ..signals(RunStatus::Running)
        };
        assert_eq!(derive_stage(&s), Some(Stage::NeedsInput));
    }

    #[test]
    fn awaiting_input_outranks_a_ready_pr() {
        let s = StageSignals {
            awaiting_input: true,
            pr: Some(PrState::ReadyToMerge),
            ..signals(RunStatus::Running)
        };
        assert_eq!(derive_stage(&s), Some(Stage::NeedsInput));
    }

    #[test]
    fn a_hibernated_run_needs_input() {
        assert_eq!(
            derive_stage(&signals(RunStatus::Hibernated)),
            Some(Stage::NeedsInput)
        );
    }

    #[test]
    fn pr_states_map_to_their_stages() {
        for (pr, stage) in [
            (PrState::ReadyToMerge, Stage::ReadyToMerge),
            (PrState::NeedsReview, Stage::NeedsReview),
            (PrState::Conflicted, Stage::Conflicted),
        ] {
            let s = StageSignals {
                pr: Some(pr),
                ..signals(RunStatus::Done)
            };
            assert_eq!(derive_stage(&s), Some(stage), "{pr:?}");
        }
    }

    #[test]
    fn a_pr_outranks_a_failed_run_and_drift() {
        let s = StageSignals {
            pr: Some(PrState::Conflicted),
            drifted: true,
            ..signals(RunStatus::Failed)
        };
        assert_eq!(derive_stage(&s), Some(Stage::Conflicted));
    }

    #[test]
    fn a_review_run_needs_review_without_a_pr_state() {
        assert_eq!(
            derive_stage(&signals(RunStatus::Review)),
            Some(Stage::NeedsReview)
        );
    }

    #[test]
    fn failed_interrupted_and_blocked_runs_are_stuck() {
        for status in [
            RunStatus::Failed,
            RunStatus::Interrupted,
            RunStatus::Blocked,
        ] {
            assert_eq!(
                derive_stage(&signals(status)),
                Some(Stage::Stuck),
                "{status:?}"
            );
        }
    }

    #[test]
    fn a_stale_running_run_is_stuck() {
        let s = StageSignals {
            heartbeat_stale: true,
            ..signals(RunStatus::Running)
        };
        assert_eq!(derive_stage(&s), Some(Stage::Stuck));
    }

    #[test]
    fn drift_makes_a_done_ticket_stuck() {
        let s = StageSignals {
            drifted: true,
            ..signals(RunStatus::Done)
        };
        assert_eq!(derive_stage(&s), Some(Stage::Stuck));
    }

    #[test]
    fn running_and_queued_runs_are_running() {
        for status in [RunStatus::Running, RunStatus::Queued] {
            assert_eq!(
                derive_stage(&signals(status)),
                Some(Stage::Running),
                "{status:?}"
            );
        }
    }

    #[test]
    fn a_ready_ticket_with_no_run_is_not_started() {
        let s = StageSignals {
            in_ready_list: true,
            ..StageSignals::default()
        };
        assert_eq!(derive_stage(&s), Some(Stage::NotStarted));
    }

    #[test]
    fn a_ready_ticket_with_a_done_run_is_not_not_started() {
        let s = StageSignals {
            in_ready_list: true,
            ..signals(RunStatus::Done)
        };
        assert_eq!(derive_stage(&s), None);
    }

    #[test]
    fn a_done_run_with_nothing_else_known_has_no_stage() {
        assert_eq!(derive_stage(&signals(RunStatus::Done)), None);
    }

    #[test]
    fn no_run_and_not_ready_has_no_stage() {
        assert_eq!(derive_stage(&StageSignals::default()), None);
    }

    // --- resolve_repo_root: ADR-0009 decision 5 ---

    #[test]
    fn a_recorded_repo_root_wins_without_asking_git() {
        let root = resolve_repo_root(Some("/src/proj"), "/wt/proj-1", &|_| {
            panic!("must not resolve from the worktree")
        });
        assert_eq!(root, RepoRoot::Recorded(PathBuf::from("/src/proj")));
        assert!(root.actions_enabled());
    }

    #[test]
    fn an_unrecorded_root_falls_back_to_the_worktree() {
        let root = resolve_repo_root(None, "/wt/proj-1", &|wt| {
            assert_eq!(wt, Path::new("/wt/proj-1"));
            Some(PathBuf::from("/src/proj"))
        });
        assert_eq!(root, RepoRoot::FromWorktree(PathBuf::from("/src/proj")));
        assert_eq!(root.path(), Some(Path::new("/src/proj")));
    }

    #[test]
    fn a_root_resolved_neither_way_disables_actions() {
        let root = resolve_repo_root(None, "/wt/gone", &no_worktree);
        assert_eq!(root, RepoRoot::Unresolved);
        assert!(!root.actions_enabled());
        assert_eq!(root.path(), None);
    }

    #[test]
    fn an_empty_worktree_is_not_resolved() {
        let root = resolve_repo_root(None, "", &|_| panic!("must not resolve ''"));
        assert_eq!(root, RepoRoot::Unresolved);
    }

    // --- join_rows ---

    #[test]
    fn collapses_a_tickets_runs_into_one_row() {
        let runs = vec![
            run(3, "GH-1", RunStatus::Running),
            run(2, "GH-1", RunStatus::Failed),
            run(1, "GH-1", RunStatus::Done),
        ];

        let rows = join_rows(&runs, &TrackerSignals::default(), 600, &no_worktree);

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, key("GH-1"));
        assert_eq!(rows[0].stage, Stage::Running);
        assert_eq!(rows[0].run_id, Some(3));
    }

    #[test]
    fn the_same_ticket_in_two_scopes_is_two_rows() {
        let mut other = run(2, "GH-1", RunStatus::Running);
        other.scope = "gh:me/other".to_string();
        let runs = vec![other, run(1, "GH-1", RunStatus::Running)];

        let rows = join_rows(&runs, &TrackerSignals::default(), 600, &no_worktree);

        let keys: Vec<_> = rows.iter().map(|r| r.key.clone()).collect();
        assert_eq!(
            keys,
            vec![
                TicketKey::new("gh:me/other", "GH-1"),
                TicketKey::new("gh:me/proj", "GH-1"),
            ]
        );
    }

    #[test]
    fn an_older_failed_run_superseded_by_a_newer_one_is_not_a_signal() {
        let runs = vec![
            run(2, "GH-1", RunStatus::Done),
            run(1, "GH-1", RunStatus::Failed),
        ];

        let rows = join_rows(&runs, &TrackerSignals::default(), 600, &no_worktree);

        assert!(rows.is_empty(), "{rows:?}");
    }

    #[test]
    fn a_live_lane_awaiting_input_is_not_hidden_by_a_newer_run() {
        let mut lane = run(1, "GH-1", RunStatus::Running);
        lane.awaiting_input = true;
        let mut audit = run(2, "GH-1", RunStatus::Done);
        audit.kind = "audit".to_string();
        let runs = vec![audit, lane];

        let rows = join_rows(&runs, &TrackerSignals::default(), 600, &no_worktree);

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].stage, Stage::NeedsInput);
        assert_eq!(rows[0].run_id, Some(1));
        assert_eq!(rows[0].kind.as_deref(), Some("lane"));
    }

    #[test]
    fn a_heartbeat_past_the_threshold_is_stuck() {
        let mut stale = run(1, "GH-1", RunStatus::Running);
        stale.heartbeat_age_secs = Some(601);
        let mut fresh = run(2, "GH-2", RunStatus::Running);
        fresh.heartbeat_age_secs = Some(600);

        let rows = join_rows(
            &[fresh, stale],
            &TrackerSignals::default(),
            600,
            &no_worktree,
        );

        let stages: Vec<_> = rows
            .iter()
            .map(|r| (r.key.ticket.as_str(), r.stage))
            .collect();
        assert_eq!(
            stages,
            vec![("GH-1", Stage::Stuck), ("GH-2", Stage::Running)]
        );
    }

    #[test]
    fn tracker_signals_layer_over_runs() {
        let mut tracker = TrackerSignals::default();
        tracker.pr.insert(key("GH-1"), PrState::ReadyToMerge);
        tracker.drifted.insert(key("GH-2"));
        let runs = vec![
            run(2, "GH-2", RunStatus::Done),
            run(1, "GH-1", RunStatus::Done),
        ];

        let rows = join_rows(&runs, &tracker, 600, &no_worktree);

        let stages: Vec<_> = rows
            .iter()
            .map(|r| (r.key.ticket.as_str(), r.stage))
            .collect();
        assert_eq!(
            stages,
            vec![("GH-1", Stage::ReadyToMerge), ("GH-2", Stage::Stuck)]
        );
    }

    #[test]
    fn ready_tickets_without_runs_are_not_started_rows() {
        let tracker = TrackerSignals {
            ready: vec![
                ReadyTicket {
                    key: key("GH-9"),
                    repo_root: PathBuf::from("/src/proj"),
                },
                ReadyTicket {
                    key: key("GH-1"),
                    repo_root: PathBuf::from("/src/proj"),
                },
            ],
            ..TrackerSignals::default()
        };
        let runs = vec![run(1, "GH-1", RunStatus::Done)];

        let rows = join_rows(&runs, &tracker, 600, &no_worktree);

        assert_eq!(rows.len(), 1, "a ticket with a run is never not-started");
        assert_eq!(rows[0].key, key("GH-9"));
        assert_eq!(rows[0].stage, Stage::NotStarted);
        assert_eq!(rows[0].run_id, None);
        assert_eq!(rows[0].stage_age_secs, 0);
        assert_eq!(
            rows[0].repo_root,
            RepoRoot::Recorded(PathBuf::from("/src/proj"))
        );
    }

    #[test]
    fn the_row_uses_the_newest_recorded_repo_root_of_any_run() {
        let mut older = run(1, "GH-1", RunStatus::Done);
        older.repo_root = Some("/src/old".to_string());
        let mut middle = run(2, "GH-1", RunStatus::Done);
        middle.repo_root = Some("/src/proj".to_string());
        let newest = run(3, "GH-1", RunStatus::Running);

        let rows = join_rows(
            &[newest, middle, older],
            &TrackerSignals::default(),
            600,
            &|_| panic!("a recorded root must win"),
        );

        assert_eq!(
            rows[0].repo_root,
            RepoRoot::Recorded(PathBuf::from("/src/proj"))
        );
    }

    #[test]
    fn an_unrecorded_row_resolves_from_the_winning_runs_worktree() {
        let rows = join_rows(
            &[run(1, "GH-1", RunStatus::Running)],
            &TrackerSignals::default(),
            600,
            &|wt| (wt == Path::new("/wt/gh-1")).then(|| PathBuf::from("/src/proj")),
        );

        assert_eq!(
            rows[0].repo_root,
            RepoRoot::FromWorktree(PathBuf::from("/src/proj"))
        );
        assert_eq!(rows[0].worktree.as_deref(), Some("/wt/gh-1"));
    }

    #[test]
    fn a_row_resolving_no_repo_root_stays_visible() {
        let rows = join_rows(
            &[run(1, "GH-1", RunStatus::Failed)],
            &TrackerSignals::default(),
            600,
            &no_worktree,
        );

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].repo_root, RepoRoot::Unresolved);
    }

    #[test]
    fn the_row_carries_the_newest_recorded_pr_url() {
        let mut lane = run(1, "GH-1", RunStatus::Done);
        lane.pr_url = Some("https://github.com/me/proj/pull/5".to_string());
        let fix = run(2, "GH-1", RunStatus::Running);

        let rows = join_rows(&[fix, lane], &TrackerSignals::default(), 600, &no_worktree);

        assert_eq!(
            rows[0].pr_url.as_deref(),
            Some("https://github.com/me/proj/pull/5")
        );
    }

    #[test]
    fn the_stage_age_is_the_winning_runs_state_age() {
        let mut r = run(1, "GH-1", RunStatus::Running);
        r.state_age_secs = 42;

        let rows = join_rows(&[r], &TrackerSignals::default(), 600, &no_worktree);

        assert_eq!(rows[0].stage_age_secs, 42);
    }

    // --- attention_order ---

    fn row(ticket: &str, stage: Stage, age: i64) -> OverviewRow {
        OverviewRow {
            key: key(ticket),
            stage,
            stage_age_secs: age,
            run_id: None,
            run_status: None,
            kind: None,
            pr_url: None,
            worktree: None,
            repo_root: RepoRoot::Unresolved,
        }
    }

    #[test]
    fn orders_by_stage_rank_first() {
        let mut rows = vec![
            row("GH-1", Stage::NotStarted, 900),
            row("GH-2", Stage::Running, 900),
            row("GH-3", Stage::NeedsInput, 1),
            row("GH-4", Stage::Stuck, 900),
            row("GH-5", Stage::ReadyToMerge, 1),
        ];

        attention_order(&mut rows);

        let order: Vec<_> = rows.iter().map(|r| r.key.ticket.as_str()).collect();
        assert_eq!(order, vec!["GH-3", "GH-5", "GH-4", "GH-2", "GH-1"]);
    }

    #[test]
    fn ties_break_oldest_in_stage_first_then_by_key() {
        let mut rows = vec![
            row("GH-1", Stage::Running, 10),
            row("GH-3", Stage::Running, 300),
            row("GH-2", Stage::Running, 300),
        ];

        attention_order(&mut rows);

        let order: Vec<_> = rows.iter().map(|r| r.key.ticket.as_str()).collect();
        assert_eq!(order, vec!["GH-2", "GH-3", "GH-1"]);
    }

    // --- active_scope_runs: ADR-0009 decision 3's polled-scope window ---

    fn scoped(id: i64, scope: &str, status: RunStatus, state_age_secs: i64) -> OverviewRun {
        OverviewRun {
            scope: scope.to_string(),
            state_age_secs,
            ..run(id, &format!("GH-{id}"), status)
        }
    }

    #[test]
    fn active_scope_runs_keeps_every_run_of_a_scope_with_a_live_run() {
        let runs = vec![
            scoped(1, "gh:me/live", RunStatus::Hibernated, 30 * DAY_SECS),
            scoped(2, "gh:me/live", RunStatus::Failed, 30 * DAY_SECS),
        ];

        let ids: Vec<i64> = active_scope_runs(&runs, ACTIVE_SCOPE_WINDOW_SECS)
            .iter()
            .map(|r| r.id)
            .collect();

        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn active_scope_runs_keeps_a_scope_whose_run_ended_inside_the_window() {
        let runs = vec![
            scoped(1, "gh:me/recent", RunStatus::Done, DAY_SECS),
            scoped(2, "gh:me/recent", RunStatus::Failed, 30 * DAY_SECS),
        ];

        assert_eq!(active_scope_runs(&runs, ACTIVE_SCOPE_WINDOW_SECS).len(), 2);
    }

    #[test]
    fn active_scope_runs_drops_a_scope_with_nothing_live_or_recent() {
        let runs = vec![
            scoped(1, "gh:me/stale", RunStatus::Failed, 8 * DAY_SECS),
            scoped(2, "gh:me/fresh", RunStatus::Running, 10),
        ];

        let ids: Vec<i64> = active_scope_runs(&runs, ACTIVE_SCOPE_WINDOW_SECS)
            .iter()
            .map(|r| r.id)
            .collect();

        assert_eq!(ids, vec![2]);
    }
}
