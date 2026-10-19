//! `tm overview`'s write actions (GitHub issue #88, ADR-0009 slice 7):
//! `V` vdiff review, `F` fix pass, `M` merge, and `Space` to queue a batch
//! merge, each routed by the highlighted row's own repo rather than the
//! cwd `tm overview` was started in (ADR-0009 decision 6).
//!
//! The board's versions of these keys assume one repo. The overview spans
//! many, so here every action:
//!
//! - needs the row's [`RepoRoot`] resolved. A row that resolves neither way
//!   keeps its actions disabled and says so in the status line (decision 5).
//! - spawns the same child `tm` invocation the board does (`tm review fix`,
//!   `tm merge`), with the repo root as its cwd.
//! - keeps the batch-merge queue per scope. Confirming fires one `tm merge
//!   K1 K2 ...` per repo, because a batch shares one conflict session
//!   (#67) and that session lives in one repo.
//!
//! `M` and `Space` are gated on PR state: a row needs a known PR, either a
//! PR stage from the tracker snapshot or a `pr_url` one of its runs
//! recorded. A conflicted PR is still allowed through, since `tm merge`
//! hands conflicts to its own resolution session. `tm merge` re-resolves
//! the PR itself, so there is no up-front PR lookup and the confirmation
//! names tickets, not PR numbers.
//!
//! Pure like the rest of [`crate::tui::app`]: handlers take and return
//! [`App`] plus the [`Cmd`]s to run.

use std::path::PathBuf;

use crate::runs::overview::{OverviewRow, RepoRoot, Stage, TicketKey};
use crate::tui::app::{App, Cmd, Screen};

/// One `tm merge` child the overview's batch confirmation will launch:
/// every queued ticket of one scope, run in that scope's repo root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverviewMergeBatch {
    /// The scope the tickets belong to.
    pub scope: String,
    /// The repo root `tm merge` runs in.
    pub repo_root: PathBuf,
    /// Ticket keys in argv order: attention-queue order, with queued keys
    /// no longer on the overview appended last.
    pub keys: Vec<String>,
}

/// The overview's pending merge confirmation (`M`), captured whole when it
/// opens so a reload can't change what confirming does. A single `M` with
/// an empty queue is one batch of one ticket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverviewMergeConfirm {
    /// One entry per repo, ordered by scope.
    pub batches: Vec<OverviewMergeBatch>,
}

impl OverviewMergeConfirm {
    /// Every ticket key the confirmation covers, joined for a status line.
    pub fn label(&self) -> String {
        self.batches
            .iter()
            .flat_map(|batch| batch.keys.iter())
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The highlighted row when an action can run on it: on
/// [`Screen::Overview`], with no confirmation open, and with a resolved
/// repo root. Otherwise `None`, after setting the status line when the
/// reason is the repo root.
fn actionable_row(app: &mut App) -> Option<(OverviewRow, PathBuf)> {
    if app.screen != Screen::Overview || app.confirm_open() {
        return None;
    }
    let row = app.selected_overview_row()?.clone();
    match &row.repo_root {
        RepoRoot::Unresolved => {
            app.status_line = format!(
                "no repo root known for {} -- actions disabled",
                row.key.ticket
            );
            None
        }
        root => {
            let path = root.path()?.to_path_buf();
            Some((row, path))
        }
    }
}

/// Why `row` can't be merged from the overview, or `None` when it can:
/// it needs a known PR, and no merge for it already in flight.
fn merge_blocker(app: &App, row: &OverviewRow) -> Option<String> {
    let ticket = &row.key.ticket;
    if app.overview_pending_merges.contains(&row.key) {
        return Some(format!("merge for {ticket} already in flight"));
    }
    let pr_stage = matches!(
        row.stage,
        Stage::ReadyToMerge | Stage::NeedsReview | Stage::Conflicted
    );
    if row.pr_url.is_none() && !pr_stage {
        return Some(format!("no PR known for {ticket}"));
    }
    None
}

/// `V` on the overview: open the row's latest run's worktree in `vdiff`,
/// resolved by the row's own scope. Needs a run.
pub fn view_diff(mut app: App) -> (App, Vec<Cmd>) {
    let Some((row, _)) = actionable_row(&mut app) else {
        return (app, Vec::new());
    };
    if row.run_id.is_none() {
        app.status_line = format!("no run for {}", row.key.ticket);
        return (app, Vec::new());
    }
    (
        app,
        vec![Cmd::ViewDiffInScope {
            scope: row.key.scope,
            key: row.key.ticket,
        }],
    )
}

/// `F` on the overview: dispatch `tm review fix <key>` in the row's repo
/// root. Needs a run, like the board's `F`.
pub fn review_fix(mut app: App) -> (App, Vec<Cmd>) {
    let Some((row, repo_root)) = actionable_row(&mut app) else {
        return (app, Vec::new());
    };
    let ticket = row.key.ticket;
    if row.run_id.is_none() {
        app.status_line = format!("no run for {ticket}");
        return (app, Vec::new());
    }
    app.status_line = format!("dispatching fix pass for {ticket}");
    (
        app,
        vec![Cmd::LaunchReviewFixIn {
            key: ticket,
            repo_root,
        }],
    )
}

/// `Space` on the overview: flip the highlighted row in or out of
/// [`App::overview_merge_queue`]. Adding is gated like `M`; removing never
/// is.
pub fn toggle_merge_queue(mut app: App) -> (App, Vec<Cmd>) {
    let Some((row, repo_root)) = actionable_row(&mut app) else {
        return (app, Vec::new());
    };
    if app.overview_merge_queue.remove(&row.key).is_some() {
        return (app, Vec::new());
    }
    if let Some(reason) = merge_blocker(&app, &row) {
        app.status_line = reason;
        return (app, Vec::new());
    }
    app.overview_merge_queue.insert(row.key, repo_root);
    (app, Vec::new())
}

/// `M` on the overview: open [`App::overview_merge_confirm`]. With a
/// non-empty queue it covers the whole queue, one batch per scope, and the
/// highlight plays no part. Otherwise it covers the highlighted row alone,
/// gated on its PR state.
pub fn merge(mut app: App) -> (App, Vec<Cmd>) {
    if !app.overview_merge_queue.is_empty() {
        if app.screen != Screen::Overview || app.confirm_open() {
            return (app, Vec::new());
        }
        let batches = queued_batches(&app);
        app.status_line = String::new();
        app.overview_merge_confirm = Some(OverviewMergeConfirm { batches });
        return (app, Vec::new());
    }
    let Some((row, repo_root)) = actionable_row(&mut app) else {
        return (app, Vec::new());
    };
    if let Some(reason) = merge_blocker(&app, &row) {
        app.status_line = reason;
        return (app, Vec::new());
    }
    app.status_line = String::new();
    app.overview_merge_confirm = Some(OverviewMergeConfirm {
        batches: vec![OverviewMergeBatch {
            scope: row.key.scope,
            repo_root,
            keys: vec![row.key.ticket],
        }],
    });
    (app, Vec::new())
}

/// [`App::overview_merge_queue`] grouped into one batch per scope, ordered
/// by scope, keys in attention-queue order with any queued key no longer on
/// the overview appended last.
fn queued_batches(app: &App) -> Vec<OverviewMergeBatch> {
    let mut ordered: Vec<&TicketKey> = app
        .overview_rows
        .iter()
        .map(|row| &row.key)
        .filter(|key| app.overview_merge_queue.contains_key(key))
        .collect();
    for key in app.overview_merge_queue.keys() {
        if !ordered.contains(&key) {
            ordered.push(key);
        }
    }
    let mut batches: Vec<OverviewMergeBatch> = Vec::new();
    for key in ordered {
        let repo_root = &app.overview_merge_queue[key];
        match batches.iter_mut().find(|batch| batch.scope == key.scope) {
            Some(batch) => batch.keys.push(key.ticket.clone()),
            None => batches.push(OverviewMergeBatch {
                scope: key.scope.clone(),
                repo_root: repo_root.clone(),
                keys: vec![key.ticket.clone()],
            }),
        }
    }
    batches.sort_by(|a, b| a.scope.cmp(&b.scope));
    batches
}

/// Accepting [`App::overview_merge_confirm`]: mark every covered ticket
/// in flight, clear the queue, and launch one `tm merge` per batch.
pub fn confirm_accept(mut app: App, confirm: OverviewMergeConfirm) -> (App, Vec<Cmd>) {
    app.status_line = format!("merging {} via tm merge...", confirm.label());
    app.overview_merge_queue.clear();
    let cmds = confirm
        .batches
        .into_iter()
        .map(|batch| {
            app.overview_pending_merges.extend(
                batch
                    .keys
                    .iter()
                    .map(|key| TicketKey::new(batch.scope.clone(), key.clone())),
            );
            Cmd::LaunchMergeIn {
                scope: batch.scope,
                repo_root: batch.repo_root,
                keys: batch.keys,
            }
        })
        .collect();
    (app, cmds)
}

/// A finished overview `tm merge` child: clear its tickets from
/// [`App::overview_pending_merges`], report the outcome, and reload the
/// overview after a merge so the rows move on.
pub fn merge_result(
    mut app: App,
    scope: String,
    keys: Vec<String>,
    merged: bool,
    message: String,
) -> (App, Vec<Cmd>) {
    for key in keys {
        app.overview_pending_merges
            .remove(&TicketKey::new(scope.clone(), key));
    }
    app.status_line = message;
    if merged {
        (app, vec![Cmd::LoadOverview])
    } else {
        (app, Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runs::RunStatus;
    use crate::tui::app::{Msg, update};

    const SCOPE_A: &str = "github:o/a";
    const SCOPE_B: &str = "github:o/b";

    fn row(scope: &str, ticket: &str, stage: Stage) -> OverviewRow {
        OverviewRow {
            key: TicketKey::new(scope, ticket),
            stage,
            stage_age_secs: 0,
            run_id: Some(1),
            run_status: Some(RunStatus::Review),
            kind: Some("lane".to_string()),
            pr_url: Some(format!("https://github.com/{scope}/pull/1")),
            worktree: Some(format!("/wt/{ticket}")),
            repo_root: RepoRoot::Recorded(root(scope)),
            auto_review_round: None,
            review_import: None,
            fix_rounds: 0,
        }
    }

    fn root(scope: &str) -> PathBuf {
        PathBuf::from(format!("/repos/{}", scope.rsplit('/').next().unwrap()))
    }

    fn app_with(rows: Vec<OverviewRow>, selected: usize) -> App {
        App {
            screen: Screen::Overview,
            overview_rows: rows,
            overview_selected: selected,
            ..App::new()
        }
    }

    fn unresolved(mut row: OverviewRow) -> OverviewRow {
        row.repo_root = RepoRoot::Unresolved;
        row
    }

    // --- shared repo-root gate ---

    #[test]
    fn every_action_is_disabled_on_a_row_with_no_repo_root() {
        for msg in [
            Msg::ViewDiffAction,
            Msg::ReviewFixAction,
            Msg::MergePrAction,
            Msg::ToggleMergeQueue,
        ] {
            let app = app_with(
                vec![unresolved(row(SCOPE_A, "GH-1", Stage::NeedsReview))],
                0,
            );
            let (app, cmds) = update(app, msg.clone());
            assert!(cmds.is_empty(), "{msg:?}");
            assert!(app.overview_merge_confirm.is_none(), "{msg:?}");
            assert!(app.overview_merge_queue.is_empty(), "{msg:?}");
            assert_eq!(
                app.status_line, "no repo root known for GH-1 -- actions disabled",
                "{msg:?}"
            );
        }
    }

    #[test]
    fn actions_with_no_rows_are_noops() {
        for msg in [
            Msg::ViewDiffAction,
            Msg::ReviewFixAction,
            Msg::MergePrAction,
            Msg::ToggleMergeQueue,
        ] {
            let (app, cmds) = update(app_with(Vec::new(), 0), msg);
            assert!(cmds.is_empty());
            assert!(app.status_line.is_empty());
        }
    }

    #[test]
    fn a_worktree_resolved_repo_root_enables_actions() {
        let mut r = row(SCOPE_A, "GH-1", Stage::Running);
        r.repo_root = RepoRoot::FromWorktree(PathBuf::from("/repos/a"));
        let (_, cmds) = update(app_with(vec![r], 0), Msg::ReviewFixAction);
        assert_eq!(
            cmds,
            vec![Cmd::LaunchReviewFixIn {
                key: "GH-1".to_string(),
                repo_root: PathBuf::from("/repos/a"),
            }]
        );
    }

    // --- V ---

    #[test]
    fn v_opens_vdiff_resolved_by_the_rows_scope() {
        let app = app_with(
            vec![
                row(SCOPE_A, "GH-1", Stage::Running),
                row(SCOPE_B, "GH-1", Stage::Running),
            ],
            1,
        );
        let (_, cmds) = update(app, Msg::ViewDiffAction);
        assert_eq!(
            cmds,
            vec![Cmd::ViewDiffInScope {
                scope: SCOPE_B.to_string(),
                key: "GH-1".to_string(),
            }]
        );
    }

    #[test]
    fn v_on_a_row_with_no_run_says_so() {
        let mut r = row(SCOPE_A, "GH-7", Stage::NotStarted);
        r.run_id = None;
        let (app, cmds) = update(app_with(vec![r], 0), Msg::ViewDiffAction);
        assert!(cmds.is_empty());
        assert_eq!(app.status_line, "no run for GH-7");
    }

    // --- F ---

    #[test]
    fn f_dispatches_a_fix_pass_in_the_rows_repo_root() {
        let app = app_with(vec![row(SCOPE_B, "GH-3", Stage::NeedsReview)], 0);
        let (app, cmds) = update(app, Msg::ReviewFixAction);
        assert_eq!(
            cmds,
            vec![Cmd::LaunchReviewFixIn {
                key: "GH-3".to_string(),
                repo_root: root(SCOPE_B),
            }]
        );
        assert_eq!(app.status_line, "dispatching fix pass for GH-3");
    }

    #[test]
    fn f_on_a_row_with_no_run_says_so() {
        let mut r = row(SCOPE_A, "GH-7", Stage::NotStarted);
        r.run_id = None;
        let (app, cmds) = update(app_with(vec![r], 0), Msg::ReviewFixAction);
        assert!(cmds.is_empty());
        assert_eq!(app.status_line, "no run for GH-7");
    }

    // --- M (single) ---

    #[test]
    fn m_opens_a_confirmation_for_the_highlighted_row_in_its_repo() {
        let app = app_with(vec![row(SCOPE_A, "GH-1", Stage::ReadyToMerge)], 0);
        let (app, cmds) = update(app, Msg::MergePrAction);
        assert!(cmds.is_empty());
        assert_eq!(
            app.overview_merge_confirm,
            Some(OverviewMergeConfirm {
                batches: vec![OverviewMergeBatch {
                    scope: SCOPE_A.to_string(),
                    repo_root: root(SCOPE_A),
                    keys: vec!["GH-1".to_string()],
                }],
            })
        );
        assert!(app.confirm_open());
    }

    #[test]
    fn m_on_a_row_with_no_known_pr_is_refused() {
        let mut r = row(SCOPE_A, "GH-1", Stage::Running);
        r.pr_url = None;
        let (app, cmds) = update(app_with(vec![r], 0), Msg::MergePrAction);
        assert!(cmds.is_empty());
        assert!(app.overview_merge_confirm.is_none());
        assert_eq!(app.status_line, "no PR known for GH-1");
    }

    /// A PR stage from the tracker snapshot is enough, even before any run
    /// recorded the PR's URL.
    #[test]
    fn m_accepts_a_pr_stage_without_a_recorded_pr_url() {
        for stage in [Stage::ReadyToMerge, Stage::NeedsReview, Stage::Conflicted] {
            let mut r = row(SCOPE_A, "GH-1", stage);
            r.pr_url = None;
            let (app, _) = update(app_with(vec![r], 0), Msg::MergePrAction);
            assert!(app.overview_merge_confirm.is_some(), "{stage:?}");
        }
    }

    #[test]
    fn m_on_a_ticket_already_merging_is_refused() {
        let mut app = app_with(vec![row(SCOPE_A, "GH-1", Stage::ReadyToMerge)], 0);
        app.overview_pending_merges
            .insert(TicketKey::new(SCOPE_A, "GH-1"));
        let (app, _) = update(app, Msg::MergePrAction);
        assert!(app.overview_merge_confirm.is_none());
        assert_eq!(app.status_line, "merge for GH-1 already in flight");
    }

    /// The same ticket key in another repo is a different ticket.
    #[test]
    fn a_merge_in_flight_in_another_scope_does_not_block() {
        let mut app = app_with(vec![row(SCOPE_A, "GH-1", Stage::ReadyToMerge)], 0);
        app.overview_pending_merges
            .insert(TicketKey::new(SCOPE_B, "GH-1"));
        let (app, _) = update(app, Msg::MergePrAction);
        assert!(app.overview_merge_confirm.is_some());
    }

    #[test]
    fn accepting_a_single_merge_launches_tm_merge_in_the_repo_root() {
        let app = app_with(vec![row(SCOPE_A, "GH-1", Stage::ReadyToMerge)], 0);
        let (app, _) = update(app, Msg::MergePrAction);
        let (app, cmds) = update(app, Msg::ConfirmAccept);
        assert_eq!(
            cmds,
            vec![Cmd::LaunchMergeIn {
                scope: SCOPE_A.to_string(),
                repo_root: root(SCOPE_A),
                keys: vec!["GH-1".to_string()],
            }]
        );
        assert!(app.overview_merge_confirm.is_none());
        assert!(
            app.overview_pending_merges
                .contains(&TicketKey::new(SCOPE_A, "GH-1"))
        );
        assert_eq!(app.status_line, "merging GH-1 via tm merge...");
    }

    #[test]
    fn cancelling_the_confirmation_launches_nothing_and_keeps_the_queue() {
        let mut app = app_with(
            vec![
                row(SCOPE_A, "GH-1", Stage::ReadyToMerge),
                row(SCOPE_A, "GH-2", Stage::ReadyToMerge),
            ],
            0,
        );
        app.overview_merge_queue
            .insert(TicketKey::new(SCOPE_A, "GH-2"), root(SCOPE_A));
        let (app, _) = update(app, Msg::MergePrAction);
        let (app, cmds) = update(app, Msg::ConfirmCancel);
        assert!(cmds.is_empty());
        assert!(app.overview_merge_confirm.is_none());
        assert_eq!(app.overview_merge_queue.len(), 1);
        assert_eq!(app.status_line, "merge of GH-2 cancelled");
    }

    // --- Awaiting review (GitHub issue #119) ---

    /// The autopilot's human stop offers the existing review actions: `V`
    /// to read the diff, `F` to send it back, `M`/`Space` to accept it.
    #[test]
    fn an_awaiting_review_row_supports_v_f_m_and_space() {
        let awaiting = || app_with(vec![row(SCOPE_A, "GH-1", Stage::AwaitingReview)], 0);

        let (_, cmds) = update(awaiting(), Msg::ViewDiffAction);
        assert!(
            matches!(cmds[..], [Cmd::ViewDiffInScope { .. }]),
            "{cmds:?}"
        );
        let (_, cmds) = update(awaiting(), Msg::ReviewFixAction);
        assert!(
            matches!(cmds[..], [Cmd::LaunchReviewFixIn { .. }]),
            "{cmds:?}"
        );
        let (app, _) = update(awaiting(), Msg::MergePrAction);
        assert!(app.overview_merge_confirm.is_some());
        let (app, _) = update(awaiting(), Msg::ToggleMergeQueue);
        assert!(
            app.overview_merge_queue
                .contains_key(&TicketKey::new(SCOPE_A, "GH-1"))
        );
    }

    // --- Space / batch ---

    #[test]
    fn space_toggles_the_highlighted_row_in_and_out_of_the_queue() {
        let app = app_with(vec![row(SCOPE_A, "GH-1", Stage::ReadyToMerge)], 0);
        let (app, cmds) = update(app, Msg::ToggleMergeQueue);
        assert!(cmds.is_empty());
        assert_eq!(
            app.overview_merge_queue
                .get(&TicketKey::new(SCOPE_A, "GH-1")),
            Some(&root(SCOPE_A))
        );
        let (app, _) = update(app, Msg::ToggleMergeQueue);
        assert!(app.overview_merge_queue.is_empty());
    }

    #[test]
    fn space_on_a_row_with_no_known_pr_is_refused() {
        let mut r = row(SCOPE_A, "GH-1", Stage::Running);
        r.pr_url = None;
        let (app, _) = update(app_with(vec![r], 0), Msg::ToggleMergeQueue);
        assert!(app.overview_merge_queue.is_empty());
        assert_eq!(app.status_line, "no PR known for GH-1");
    }

    #[test]
    fn space_does_not_touch_the_boards_queue() {
        let app = app_with(vec![row(SCOPE_A, "GH-1", Stage::ReadyToMerge)], 0);
        let (app, _) = update(app, Msg::ToggleMergeQueue);
        assert!(app.merge_queue.is_empty());
    }

    #[test]
    fn m_with_a_queue_groups_one_batch_per_scope_in_queue_order() {
        let mut app = app_with(
            vec![
                row(SCOPE_B, "GH-9", Stage::ReadyToMerge),
                row(SCOPE_A, "GH-10", Stage::ReadyToMerge),
                row(SCOPE_A, "GH-2", Stage::NeedsReview),
                row(SCOPE_B, "GH-1", Stage::NeedsReview),
            ],
            0,
        );
        for i in 0..4 {
            app.overview_selected = i;
            app = update(app, Msg::ToggleMergeQueue).0;
        }
        // The highlight plays no part once the queue is non-empty.
        app.overview_selected = 3;
        let (app, _) = update(app, Msg::MergePrAction);
        assert_eq!(
            app.overview_merge_confirm,
            Some(OverviewMergeConfirm {
                batches: vec![
                    OverviewMergeBatch {
                        scope: SCOPE_A.to_string(),
                        repo_root: root(SCOPE_A),
                        keys: vec!["GH-10".to_string(), "GH-2".to_string()],
                    },
                    OverviewMergeBatch {
                        scope: SCOPE_B.to_string(),
                        repo_root: root(SCOPE_B),
                        keys: vec!["GH-9".to_string(), "GH-1".to_string()],
                    },
                ],
            })
        );
    }

    #[test]
    fn a_queued_ticket_gone_from_the_overview_still_merges_last() {
        let mut app = app_with(vec![row(SCOPE_A, "GH-2", Stage::ReadyToMerge)], 0);
        app.overview_merge_queue
            .insert(TicketKey::new(SCOPE_A, "GH-1"), root(SCOPE_A));
        app.overview_merge_queue
            .insert(TicketKey::new(SCOPE_A, "GH-2"), root(SCOPE_A));
        let (app, _) = update(app, Msg::MergePrAction);
        let confirm = app.overview_merge_confirm.unwrap();
        assert_eq!(confirm.batches[0].keys, vec!["GH-2", "GH-1"]);
    }

    #[test]
    fn accepting_a_batch_fires_one_tm_merge_per_repo_and_clears_the_queue() {
        let mut app = app_with(Vec::new(), 0);
        app.overview_merge_confirm = Some(OverviewMergeConfirm {
            batches: vec![
                OverviewMergeBatch {
                    scope: SCOPE_A.to_string(),
                    repo_root: root(SCOPE_A),
                    keys: vec!["GH-1".to_string(), "GH-2".to_string()],
                },
                OverviewMergeBatch {
                    scope: SCOPE_B.to_string(),
                    repo_root: root(SCOPE_B),
                    keys: vec!["GH-1".to_string()],
                },
            ],
        });
        app.overview_merge_queue
            .insert(TicketKey::new(SCOPE_A, "GH-1"), root(SCOPE_A));
        let (app, cmds) = update(app, Msg::ConfirmAccept);
        assert_eq!(
            cmds,
            vec![
                Cmd::LaunchMergeIn {
                    scope: SCOPE_A.to_string(),
                    repo_root: root(SCOPE_A),
                    keys: vec!["GH-1".to_string(), "GH-2".to_string()],
                },
                Cmd::LaunchMergeIn {
                    scope: SCOPE_B.to_string(),
                    repo_root: root(SCOPE_B),
                    keys: vec!["GH-1".to_string()],
                },
            ]
        );
        assert!(app.overview_merge_queue.is_empty());
        assert_eq!(app.overview_pending_merges.len(), 3);
    }

    #[test]
    fn space_and_m_are_inert_while_the_confirmation_is_open() {
        let app = app_with(
            vec![
                row(SCOPE_A, "GH-1", Stage::ReadyToMerge),
                row(SCOPE_A, "GH-2", Stage::ReadyToMerge),
            ],
            0,
        );
        let (mut app, _) = update(app, Msg::MergePrAction);
        let before = app.overview_merge_confirm.clone();
        app.overview_selected = 1;
        let (app, _) = update(app, Msg::ToggleMergeQueue);
        assert!(app.overview_merge_queue.is_empty());
        let (app, _) = update(app, Msg::MergePrAction);
        assert_eq!(app.overview_merge_confirm, before);
    }

    // --- merge result ---

    #[test]
    fn a_finished_merge_clears_its_scopes_tickets_and_reloads() {
        let mut app = app_with(Vec::new(), 0);
        app.overview_pending_merges
            .insert(TicketKey::new(SCOPE_A, "GH-1"));
        app.overview_pending_merges
            .insert(TicketKey::new(SCOPE_B, "GH-1"));
        let (app, cmds) = update(
            app,
            Msg::OverviewMergeResult {
                scope: SCOPE_A.to_string(),
                keys: vec!["GH-1".to_string()],
                merged: true,
                message: "merged GH-1 via tm merge".to_string(),
            },
        );
        assert_eq!(cmds, vec![Cmd::LoadOverview]);
        assert_eq!(
            app.overview_pending_merges.iter().collect::<Vec<_>>(),
            vec![&TicketKey::new(SCOPE_B, "GH-1")]
        );
        assert_eq!(app.status_line, "merged GH-1 via tm merge");
    }

    #[test]
    fn a_failed_merge_reports_without_reloading() {
        let (app, cmds) = update(
            app_with(Vec::new(), 0),
            Msg::OverviewMergeResult {
                scope: SCOPE_A.to_string(),
                keys: vec!["GH-1".to_string()],
                merged: false,
                message: "conflicts handed back".to_string(),
            },
        );
        assert!(cmds.is_empty());
        assert_eq!(app.status_line, "conflicts handed back");
    }

    // --- the board keeps its own behaviour ---

    #[test]
    fn the_board_versions_of_the_keys_ignore_overview_state() {
        let mut app = App::new();
        app.overview_rows = vec![row(SCOPE_A, "GH-1", Stage::ReadyToMerge)];
        let (app, cmds) = update(app, Msg::ReviewFixAction);
        assert!(cmds.is_empty());
        assert!(app.overview_merge_confirm.is_none());
    }
}
