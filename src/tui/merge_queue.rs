//! The merge queue view on `tm board` and `tm overview` (GitHub issue
//! #247).
//!
//! The merge queue lives in runs.db ([`crate::runs::merge_queue`]) and the
//! automerge sweep drains it ([`crate::work::auto_merge`]). This module is
//! the TUI's window onto it:
//!
//! - `+` on the board or the overview adds the highlighted ticket to its
//!   scope's queue, or removes it when it is already queued
//!   ([`toggle_ticket`]).
//! - `Q` opens [`Screen::MergeQueue`] ([`open`]): the board's scope, or
//!   every scope the overview shows plus any other scope with queued
//!   merges. Each scope lists its merge epic and its rows in merge order,
//!   with how each row got there and its
//!   [`crate::work::auto_merge::RowState`].
//! - On that screen `Space`/`Enter` grabs a row and drops it after `j`/`k`
//!   moved it within its scope ([`grab_toggle`], the same grab-and-drop as
//!   [`Screen::Rank`]); `x` removes it; `E` clears the scope's merge epic.
//! - `m` in the board's epic picker sets or clears the scope's merge epic
//!   ([`set_epic_from_picker`]), the way `P` there sets the priority epic.
//!
//! These are distinct from the board's and overview's `Space`/`M` merge
//! *batches* (GitHub issues #68, #88), which live in TUI memory and merge
//! at once.
//!
//! Pure like the rest of [`crate::tui::app`]: handlers take and return
//! [`App`] plus the [`Cmd`]s to run. [`execute`] runs those [`Cmd`]s
//! against the run store for both event loops.

use std::path::Path;

use crate::runs::merge_queue::QueueSource;
use crate::runs::{RunStore, RunStoreError};
use crate::tui::app::{App, Cmd, EpicFilter, Msg, Screen};
use crate::work::auto_merge::{RowState, row_state};

/// One queued ticket as [`Screen::MergeQueue`] shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueRow {
    /// Ticket key.
    pub ticket: String,
    /// How it got into the queue.
    pub source: QueueSource,
    /// What it is doing.
    pub state: RowState,
}

/// One scope's merge epic and queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeQueue {
    /// The scope.
    pub scope: String,
    /// The scope's merge epic, if set.
    pub merge_epic: Option<String>,
    /// Its queued tickets, first to merge first.
    pub rows: Vec<QueueRow>,
}

/// [`Screen::MergeQueue`]'s state, kept in [`App::merge_queue_view`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergeQueueView {
    /// The scopes shown, as last loaded.
    pub scopes: Vec<ScopeQueue>,
    /// Index of the highlighted row across every scope's rows, in display
    /// order.
    pub selected: usize,
    /// Where the highlighted row started when it was grabbed, or `None`
    /// when nothing is grabbed.
    pub grab_origin: Option<usize>,
    /// `scopes` as they were at grab time, restored if the grab is
    /// cancelled.
    pub snapshot: Option<Vec<ScopeQueue>>,
    /// The screen `Esc`/`q` returns to.
    pub return_to: Screen,
    /// The scopes the view was opened for.
    pub requested: Vec<String>,
    /// Whether to also show every other scope with queued merges (opened
    /// from the overview).
    pub all: bool,
    /// The last load's error, shown in place of the rows.
    pub error: Option<String>,
}

impl MergeQueueView {
    /// Every row as `(scope index, row index)`, in display order.
    pub fn flat(&self) -> Vec<(usize, usize)> {
        self.scopes
            .iter()
            .enumerate()
            .flat_map(|(s, scope)| (0..scope.rows.len()).map(move |r| (s, r)))
            .collect()
    }

    /// The highlighted row's scope and the row, if any.
    pub fn selected_row(&self) -> Option<(&ScopeQueue, &QueueRow)> {
        let (s, r) = *self.flat().get(self.selected)?;
        Some((&self.scopes[s], &self.scopes[s].rows[r]))
    }

    /// Whether a row is grabbed.
    pub fn is_grabbed(&self) -> bool {
        self.grab_origin.is_some()
    }

    /// The scope `E` acts on: the highlighted row's, else the first shown.
    fn selected_scope(&self) -> Option<&ScopeQueue> {
        self.selected_row()
            .map(|(scope, _)| scope)
            .or_else(|| self.scopes.first())
    }

    fn load_cmd(&self) -> Cmd {
        Cmd::LoadMergeQueue {
            scopes: self.requested.clone(),
            all: self.all,
        }
    }
}

/// `Q`: open [`Screen::MergeQueue`] for the board's scope, or for the
/// overview's scopes plus every other scope with queued merges.
pub fn open(mut app: App) -> (App, Vec<Cmd>) {
    let (requested, all) = match app.screen {
        Screen::Board => match &app.autopilot_scope {
            Some(scope) => (vec![scope.clone()], false),
            None => {
                app.status_line = "the merge queue needs this repo's config to load".to_string();
                return (app, Vec::new());
            }
        },
        Screen::Overview => {
            let mut scopes: Vec<String> = app
                .overview_rows
                .iter()
                .map(|row| row.key.scope.clone())
                .filter(|scope| !scope.is_empty())
                .collect();
            scopes.sort();
            scopes.dedup();
            (scopes, true)
        }
        _ => return (app, Vec::new()),
    };
    app.merge_queue_view = MergeQueueView {
        return_to: app.screen,
        requested,
        all,
        ..MergeQueueView::default()
    };
    app.screen = Screen::MergeQueue;
    let cmd = app.merge_queue_view.load_cmd();
    (app, vec![cmd])
}

/// The reload [`Cmd`] for the merge queue view, or `None` off it or while
/// a row is grabbed (a reload would discard the pending move).
pub fn reload_cmd(app: &App) -> Option<Cmd> {
    (app.screen == Screen::MergeQueue && !app.merge_queue_view.is_grabbed())
        .then(|| app.merge_queue_view.load_cmd())
}

/// [`Cmd::LoadMergeQueue`] finished: show the rows, keeping the highlighted
/// ticket highlighted when it is still queued. Ignored while a row is
/// grabbed.
pub fn loaded(mut app: App, result: Result<Vec<ScopeQueue>, String>) -> App {
    let view = &mut app.merge_queue_view;
    if view.is_grabbed() {
        return app;
    }
    match result {
        Ok(scopes) => {
            let keep = view
                .selected_row()
                .map(|(scope, row)| (scope.scope.clone(), row.ticket.clone()));
            view.scopes = scopes;
            view.error = None;
            let flat = view.flat();
            view.selected = keep
                .and_then(|(scope, ticket)| {
                    flat.iter().position(|&(s, r)| {
                        view.scopes[s].scope == scope && view.scopes[s].rows[r].ticket == ticket
                    })
                })
                .unwrap_or_else(|| view.selected.min(flat.len().saturating_sub(1)));
        }
        Err(err) => view.error = Some(err),
    }
    app
}

/// `+` on the board or the overview: add the highlighted ticket to its
/// scope's merge queue, or remove it when it is already queued.
pub fn toggle_ticket(mut app: App) -> (App, Vec<Cmd>) {
    if app.confirm_open() {
        return (app, Vec::new());
    }
    let target = match app.screen {
        Screen::Board => match (&app.autopilot_scope, app.selected_ticket()) {
            (None, _) => Err("the merge queue needs this repo's config to load"),
            (_, None) => Err("merge queue: no ticket selected"),
            (Some(scope), Some(ticket)) => Ok((scope.clone(), ticket.key.clone(), None)),
        },
        Screen::Overview => match app.selected_overview_row() {
            None => Err("merge queue: no row selected"),
            Some(row) if row.key.scope.is_empty() => Err("merge queue: this row has no scope"),
            Some(row) => Ok((
                row.key.scope.clone(),
                row.key.ticket.clone(),
                row.repo_root.path().map(Path::to_path_buf),
            )),
        },
        _ => return (app, Vec::new()),
    };
    match target {
        Ok((scope, ticket, repo_dir)) => (
            app,
            vec![Cmd::ToggleQueuedMerge {
                scope,
                ticket,
                repo_dir,
            }],
        ),
        Err(message) => {
            app.status_line = message.to_string();
            (app, Vec::new())
        }
    }
}

/// `Space`/`Enter` on [`Screen::MergeQueue`]: grab the highlighted row, or
/// drop the grabbed one where it now is.
pub fn grab_toggle(mut app: App) -> (App, Vec<Cmd>) {
    let view = &mut app.merge_queue_view;
    let Some(origin) = view.grab_origin else {
        if view.selected_row().is_some() {
            view.grab_origin = Some(view.selected);
            view.snapshot = Some(view.scopes.clone());
        }
        return (app, Vec::new());
    };
    view.grab_origin = None;
    view.snapshot = None;
    if origin == view.selected {
        return (app, Vec::new());
    }
    let Some(&(s, r)) = view.flat().get(view.selected) else {
        return (app, Vec::new());
    };
    let cmd = Cmd::MoveQueuedMerge {
        scope: view.scopes[s].scope.clone(),
        ticket: view.scopes[s].rows[r].ticket.clone(),
        index: r,
    };
    (app, vec![cmd])
}

/// `j`/`k` on [`Screen::MergeQueue`]: move the highlight by `delta`, or
/// while a row is grabbed move the row itself, within its scope.
pub fn move_by(app: &mut App, delta: isize) {
    let view = &mut app.merge_queue_view;
    let flat = view.flat();
    if flat.is_empty() {
        return;
    }
    let Some(target) = view.selected.checked_add_signed(delta) else {
        return;
    };
    if target >= flat.len() {
        return;
    }
    if view.is_grabbed() {
        let ((s, r), (s2, r2)) = (flat[view.selected], flat[target]);
        if s != s2 {
            return;
        }
        view.scopes[s].rows.swap(r, r2);
    }
    view.selected = target;
}

/// `Esc`/`q` on [`Screen::MergeQueue`]: cancel a grab, restoring the order
/// it started from, or go back to the screen the view was opened from.
pub fn back(app: &mut App) {
    let view = &mut app.merge_queue_view;
    if let Some(origin) = view.grab_origin.take() {
        if let Some(snapshot) = view.snapshot.take() {
            view.scopes = snapshot;
        }
        view.selected = origin;
    } else {
        app.screen = view.return_to;
    }
}

/// `x` on [`Screen::MergeQueue`]: remove the highlighted row.
pub fn remove(mut app: App) -> (App, Vec<Cmd>) {
    if app.merge_queue_view.is_grabbed() {
        return (app, Vec::new());
    }
    let Some((scope, row)) = app.merge_queue_view.selected_row() else {
        app.status_line = "merge queue: nothing selected".to_string();
        return (app, Vec::new());
    };
    let cmd = Cmd::RemoveQueuedMerge {
        scope: scope.scope.clone(),
        ticket: row.ticket.clone(),
    };
    (app, vec![cmd])
}

/// `E` on [`Screen::MergeQueue`]: clear the merge epic of the highlighted
/// row's scope.
pub fn clear_epic(mut app: App) -> (App, Vec<Cmd>) {
    if app.merge_queue_view.is_grabbed() {
        return (app, Vec::new());
    }
    match app.merge_queue_view.selected_scope() {
        Some(scope) if scope.merge_epic.is_some() => {
            let scope = scope.scope.clone();
            (app, vec![Cmd::SetMergeEpic { scope, epic: None }])
        }
        _ => {
            app.status_line = "merge queue: no merge epic to clear".to_string();
            (app, Vec::new())
        }
    }
}

/// `m` in the board's epic picker: make the highlighted epic the scope's
/// merge epic, or clear it when the highlighted row is the merge epic or
/// `All epics`. The picker stays open and the view filter is untouched.
/// The current merge epic is the one the board last loaded with the
/// autopilot status ([`App::merge_epic`]).
pub fn set_epic_from_picker(mut app: App) -> (App, Vec<Cmd>) {
    let Some(scope) = app.autopilot_scope.clone() else {
        app.status_line = "the merge queue needs this repo's config to load".to_string();
        return (app, Vec::new());
    };
    let epic = match app.epic_options().get(app.epic_picker_selected) {
        Some(EpicFilter::Epic(epic)) if app.merge_epic() != Some(epic.key.as_str()) => {
            Some(epic.key.clone())
        }
        Some(EpicFilter::Epic(_) | EpicFilter::All) => None,
        Some(EpicFilter::NoEpic) | None => {
            app.status_line = "merge epic: pick an epic, or All epics to clear".to_string();
            return (app, Vec::new());
        }
    };
    (app, vec![Cmd::SetMergeEpic { scope, epic }])
}

/// A merge queue [`Cmd`] finished: show what happened, remember a new
/// merge epic, and reload the view when it is open.
pub fn changed(mut app: App, result: Result<Change, String>) -> (App, Vec<Cmd>) {
    match result {
        Ok(change) => {
            if let Change::EpicSet { scope, epic } = &change
                && let Some(status) = app.autopilot.iter_mut().find(|s| &s.scope == scope)
            {
                status.merge_epic = epic.clone();
            }
            app.status_line = change.to_string();
        }
        Err(err) => app.status_line = format!("merge queue: {err}"),
    }
    let cmds = reload_cmd(&app).into_iter().collect();
    (app, cmds)
}

/// What a merge queue write did, for the status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// `ticket` was added at `position` (1-based).
    Queued {
        /// Ticket key.
        ticket: String,
        /// Its place in the queue.
        position: usize,
    },
    /// `ticket` was removed.
    Removed {
        /// Ticket key.
        ticket: String,
    },
    /// `ticket` moved to `position` (1-based).
    Moved {
        /// Ticket key.
        ticket: String,
        /// Its new place.
        position: usize,
    },
    /// `scope`'s merge epic changed.
    EpicSet {
        /// The scope.
        scope: String,
        /// The new merge epic, or `None` when cleared.
        epic: Option<String>,
    },
}

impl std::fmt::Display for Change {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Change::Queued { ticket, position } => {
                write!(f, "queued {ticket} to merge (#{position})")
            }
            Change::Removed { ticket } => write!(f, "removed {ticket} from the merge queue"),
            Change::Moved { ticket, position } => {
                write!(f, "moved {ticket} to #{position} in the merge queue")
            }
            Change::EpicSet {
                scope,
                epic: Some(epic),
            } => write!(f, "merge epic {epic} for {scope}"),
            Change::EpicSet { scope, epic: None } => {
                write!(f, "merge epic cleared for {scope}")
            }
        }
    }
}

/// Reads the merge queues of `scopes`, plus (with `all`) every other scope
/// with queued merges, sorted by scope.
///
/// # Errors
///
/// Any run-store failure.
pub fn load(
    store: &RunStore,
    scopes: &[String],
    all: bool,
) -> Result<Vec<ScopeQueue>, RunStoreError> {
    let mut wanted = scopes.to_vec();
    if all {
        wanted.extend(store.merge_queue_scopes()?);
    }
    wanted.sort();
    wanted.dedup();
    let runs = store.overview_runs()?;
    wanted
        .into_iter()
        .map(|scope| {
            let rows = store
                .merge_queue(&scope)?
                .iter()
                .map(|entry| QueueRow {
                    ticket: entry.ticket.clone(),
                    source: entry.source,
                    state: row_state(entry, &runs),
                })
                .collect();
            Ok(ScopeQueue {
                merge_epic: store.merge_epic(&scope)?,
                scope,
                rows,
            })
        })
        .collect()
}

/// Runs one merge queue [`Cmd`] against `store`. `default_repo` is the
/// checkout to record for a [`Cmd::ToggleQueuedMerge`] that names none
/// (the board's repo), so `tm dispatch tick --all` can drain the queue.
pub fn execute(store: Option<&RunStore>, default_repo: Option<&Path>, cmd: Cmd) -> Vec<Msg> {
    let Some(store) = store else {
        let err = "the runs DB is unavailable".to_string();
        return vec![match cmd {
            Cmd::LoadMergeQueue { .. } => Msg::MergeQueueLoaded(Err(err)),
            _ => Msg::MergeQueueChanged(Err(err)),
        }];
    };
    let text = |err: RunStoreError| err.to_string();
    match cmd {
        Cmd::LoadMergeQueue { scopes, all } => {
            vec![Msg::MergeQueueLoaded(
                load(store, &scopes, all).map_err(text),
            )]
        }
        Cmd::ToggleQueuedMerge {
            scope,
            ticket,
            repo_dir,
        } => vec![Msg::MergeQueueChanged(
            toggle(store, &scope, &ticket, repo_dir.as_deref().or(default_repo)).map_err(text),
        )],
        Cmd::MoveQueuedMerge {
            scope,
            ticket,
            index,
        } => {
            let result = store
                .move_merge(&scope, &ticket, index)
                .map(|_| Change::Moved {
                    ticket,
                    position: index + 1,
                })
                .map_err(text);
            vec![Msg::MergeQueueChanged(result)]
        }
        Cmd::RemoveQueuedMerge { scope, ticket } => {
            let result = store
                .dequeue_merge(&scope, &ticket)
                .map(|_| Change::Removed { ticket })
                .map_err(text);
            vec![Msg::MergeQueueChanged(result)]
        }
        Cmd::SetMergeEpic { scope, epic } => {
            let result = store
                .set_merge_epic(&scope, epic.as_deref())
                .map(|()| Change::EpicSet { scope, epic })
                .map_err(text);
            vec![Msg::MergeQueueChanged(result)]
        }
        other => {
            debug_assert!(
                false,
                "merge_queue::execute: not a merge queue Cmd: {other:?}"
            );
            Vec::new()
        }
    }
}

fn toggle(
    store: &RunStore,
    scope: &str,
    ticket: &str,
    repo_dir: Option<&Path>,
) -> Result<Change, RunStoreError> {
    if store.dequeue_merge(scope, ticket)? {
        return Ok(Change::Removed {
            ticket: ticket.to_string(),
        });
    }
    if let Some(dir) = repo_dir {
        store.record_autopilot_repo(scope, &dir.display().to_string())?;
    }
    store.enqueue_merge(scope, ticket, QueueSource::Manual)?;
    let position = store
        .merge_queue(scope)?
        .iter()
        .position(|entry| entry.ticket == ticket)
        .map_or(0, |i| i + 1);
    Ok(Change::Queued {
        ticket: ticket.to_string(),
        position,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocker_stacking::Readiness;
    use crate::runs::overview::{OverviewRow, RepoRoot, Stage, TicketKey};
    use crate::ticketing::types::ParentRef;
    use crate::tui::app::{Column, TicketSummary};
    use crate::work::autopilot::AutopilotStatus;
    use std::path::PathBuf;
    use tempfile::{TempDir, tempdir};

    const SCOPE: &str = "github:o/r";

    fn ticket(key: &str) -> TicketSummary {
        TicketSummary {
            key: key.to_string(),
            summary: String::new(),
            status: "To Do".to_string(),
            url: String::new(),
            description: String::new(),
            status_category: "new".to_string(),
            assignee: None,
            readiness: Readiness::Unknown,
            epic: None,
        }
    }

    fn board(keys: &[&str]) -> App {
        App {
            columns: vec![Column {
                title: "To Do".to_string(),
                tickets: keys.iter().map(|k| ticket(k)).collect(),
            }],
            autopilot_scope: Some(SCOPE.to_string()),
            ..App::new()
        }
    }

    fn row(ticket: &str) -> QueueRow {
        QueueRow {
            ticket: ticket.to_string(),
            source: QueueSource::Manual,
            state: RowState::Waiting(None),
        }
    }

    fn scope(name: &str, tickets: &[&str]) -> ScopeQueue {
        ScopeQueue {
            scope: name.to_string(),
            merge_epic: None,
            rows: tickets.iter().map(|t| row(t)).collect(),
        }
    }

    /// The merge queue screen showing `scopes`, opened from the board.
    fn viewing(scopes: Vec<ScopeQueue>) -> App {
        let (app, _) = open(board(&[]));
        loaded(app, Ok(scopes))
    }

    fn order(app: &App) -> Vec<Vec<String>> {
        app.merge_queue_view
            .scopes
            .iter()
            .map(|s| s.rows.iter().map(|r| r.ticket.clone()).collect())
            .collect()
    }

    #[test]
    fn q_on_the_board_opens_the_view_for_the_boards_scope() {
        let (app, cmds) = open(board(&["GH-1"]));
        assert_eq!(app.screen, Screen::MergeQueue);
        assert_eq!(app.merge_queue_view.return_to, Screen::Board);
        assert_eq!(
            cmds,
            vec![Cmd::LoadMergeQueue {
                scopes: vec![SCOPE.to_string()],
                all: false,
            }]
        );
    }

    #[test]
    fn q_on_the_overview_also_asks_for_every_queued_scope() {
        let rows = ["github:o/b", "github:o/a", "github:o/b"]
            .iter()
            .map(|s| overview_row(s, "GH-1"))
            .collect();
        let app = App {
            screen: Screen::Overview,
            overview_rows: rows,
            ..App::new()
        };
        let (app, cmds) = open(app);
        assert_eq!(app.merge_queue_view.return_to, Screen::Overview);
        assert_eq!(
            cmds,
            vec![Cmd::LoadMergeQueue {
                scopes: vec!["github:o/a".to_string(), "github:o/b".to_string()],
                all: true,
            }]
        );
    }

    fn overview_row(scope: &str, ticket: &str) -> OverviewRow {
        OverviewRow {
            key: TicketKey::new(scope, ticket),
            stage: Stage::AwaitingReview,
            stage_age_secs: 0,
            run_id: Some(1),
            run_status: None,
            kind: Some("lane".to_string()),
            pr_url: None,
            worktree: None,
            repo_root: RepoRoot::Recorded(PathBuf::from("/repos/r")),
            auto_review_round: None,
            review_import: None,
            fix_rounds: 0,
            awaiting_reply: false,
        }
    }

    #[test]
    fn plus_on_the_board_toggles_the_selected_ticket() {
        let (_, cmds) = toggle_ticket(board(&["GH-7"]));
        assert_eq!(
            cmds,
            vec![Cmd::ToggleQueuedMerge {
                scope: SCOPE.to_string(),
                ticket: "GH-7".to_string(),
                repo_dir: None,
            }]
        );
    }

    #[test]
    fn plus_on_the_overview_passes_the_rows_repo() {
        let app = App {
            screen: Screen::Overview,
            overview_rows: vec![overview_row(SCOPE, "GH-3")],
            ..App::new()
        };
        let (_, cmds) = toggle_ticket(app);
        assert_eq!(
            cmds,
            vec![Cmd::ToggleQueuedMerge {
                scope: SCOPE.to_string(),
                ticket: "GH-3".to_string(),
                repo_dir: Some(PathBuf::from("/repos/r")),
            }]
        );
    }

    #[test]
    fn plus_without_a_scope_says_why() {
        let app = App {
            autopilot_scope: None,
            ..board(&["GH-7"])
        };
        let (app, cmds) = toggle_ticket(app);
        assert!(cmds.is_empty());
        assert!(app.status_line.contains("config"), "{}", app.status_line);
    }

    #[test]
    fn grab_move_and_drop_moves_within_the_scope() {
        let mut app = viewing(vec![
            scope("github:o/a", &["GH-1", "GH-2", "GH-3"]),
            scope("github:o/b", &["GH-9"]),
        ]);
        let (next, cmds) = grab_toggle(app);
        app = next;
        assert!(cmds.is_empty());
        assert!(app.merge_queue_view.is_grabbed());

        move_by(&mut app, 1);
        move_by(&mut app, 1);
        // Can't cross into the next scope.
        move_by(&mut app, 1);
        assert_eq!(
            order(&app),
            vec![vec!["GH-2", "GH-3", "GH-1"], vec!["GH-9"]]
        );
        assert_eq!(app.merge_queue_view.selected, 2);

        let (app, cmds) = grab_toggle(app);
        assert!(!app.merge_queue_view.is_grabbed());
        assert_eq!(
            cmds,
            vec![Cmd::MoveQueuedMerge {
                scope: "github:o/a".to_string(),
                ticket: "GH-1".to_string(),
                index: 2,
            }]
        );
    }

    #[test]
    fn dropping_in_place_sends_nothing_and_esc_cancels_a_grab() {
        let app = viewing(vec![scope(SCOPE, &["GH-1", "GH-2"])]);
        let (app, _) = grab_toggle(app);
        let (app, cmds) = grab_toggle(app);
        assert!(cmds.is_empty());

        let (mut app, _) = grab_toggle(app);
        move_by(&mut app, 1);
        back(&mut app);
        assert!(!app.merge_queue_view.is_grabbed());
        assert_eq!(order(&app), vec![vec!["GH-1", "GH-2"]]);
        assert_eq!(app.merge_queue_view.selected, 0);
        assert_eq!(app.screen, Screen::MergeQueue);

        back(&mut app);
        assert_eq!(app.screen, Screen::Board);
    }

    #[test]
    fn a_reload_keeps_the_highlighted_ticket_and_waits_out_a_grab() {
        let mut app = viewing(vec![scope(SCOPE, &["GH-1", "GH-2"])]);
        move_by(&mut app, 1);
        let app = loaded(app, Ok(vec![scope(SCOPE, &["GH-0", "GH-1", "GH-2"])]));
        assert_eq!(app.merge_queue_view.selected, 2);

        let (app, _) = grab_toggle(app);
        assert_eq!(reload_cmd(&app), None);
        let app = loaded(app, Ok(vec![scope(SCOPE, &[])]));
        assert_eq!(order(&app), vec![vec!["GH-0", "GH-1", "GH-2"]]);
    }

    #[test]
    fn x_removes_and_e_clears_the_merge_epic() {
        let app = viewing(vec![ScopeQueue {
            merge_epic: Some("GH-100".to_string()),
            ..scope(SCOPE, &["GH-1"])
        }]);
        let (app, cmds) = remove(app);
        assert_eq!(
            cmds,
            vec![Cmd::RemoveQueuedMerge {
                scope: SCOPE.to_string(),
                ticket: "GH-1".to_string(),
            }]
        );
        let (_, cmds) = clear_epic(app);
        assert_eq!(
            cmds,
            vec![Cmd::SetMergeEpic {
                scope: SCOPE.to_string(),
                epic: None,
            }]
        );
    }

    #[test]
    fn m_in_the_epic_picker_sets_then_clears_the_merge_epic() {
        let mut app = board(&[]);
        app.columns[0].tickets.push(TicketSummary {
            epic: Some(ParentRef {
                key: "GH-100".to_string(),
                summary: "Epic".to_string(),
            }),
            ..ticket("GH-1")
        });
        // All, No epic, GH-100
        app.epic_picker_selected = 2;
        let (mut app, cmds) = set_epic_from_picker(app);
        assert_eq!(
            cmds,
            vec![Cmd::SetMergeEpic {
                scope: SCOPE.to_string(),
                epic: Some("GH-100".to_string()),
            }]
        );

        app.autopilot.push(AutopilotStatus {
            scope: SCOPE.to_string(),
            enabled: false,
            running_lanes: Vec::new(),
            max_lanes: 1,
            last_refusal: None,
            blocked_age_secs: None,
            last_launch_age_secs: None,
            priority_epic: None,
            merge_epic: None,
        });
        let (app, _) = changed(
            app,
            Ok(Change::EpicSet {
                scope: SCOPE.to_string(),
                epic: Some("GH-100".to_string()),
            }),
        );
        assert_eq!(app.merge_epic(), Some("GH-100"));
        assert_eq!(app.status_line, "merge epic GH-100 for github:o/r");

        let (_, cmds) = set_epic_from_picker(app);
        assert_eq!(
            cmds,
            vec![Cmd::SetMergeEpic {
                scope: SCOPE.to_string(),
                epic: None,
            }]
        );
    }

    // --- execute ---

    fn store() -> (TempDir, RunStore) {
        let dir = tempdir().unwrap();
        let store = RunStore::open(&dir.path().join("runs.db")).unwrap();
        (dir, store)
    }

    fn change(msgs: Vec<Msg>) -> Change {
        match msgs.as_slice() {
            [Msg::MergeQueueChanged(Ok(change))] => change.clone(),
            other => panic!("expected a change, got {other:?}"),
        }
    }

    #[test]
    fn toggle_queues_then_removes_and_records_the_repo() {
        let (_dir, store) = store();
        let toggle = || Cmd::ToggleQueuedMerge {
            scope: SCOPE.to_string(),
            ticket: "GH-1".to_string(),
            repo_dir: None,
        };

        let queued = change(execute(Some(&store), Some(Path::new("/repos/r")), toggle()));
        assert_eq!(queued.to_string(), "queued GH-1 to merge (#1)");
        assert_eq!(
            store.autopilot_scopes().unwrap()[0].repo_dir.as_deref(),
            Some("/repos/r")
        );

        let removed = change(execute(Some(&store), None, toggle()));
        assert_eq!(removed.to_string(), "removed GH-1 from the merge queue");
        assert!(store.merge_queue(SCOPE).unwrap().is_empty());
    }

    #[test]
    fn load_reads_requested_and_queued_scopes_with_row_state() {
        let (_dir, store) = store();
        store
            .enqueue_merge("github:o/z", "GH-5", QueueSource::Autopilot)
            .unwrap();
        store.set_merge_epic(SCOPE, Some("GH-100")).unwrap();

        let only = load(&store, &[SCOPE.to_string()], false).unwrap();
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].merge_epic.as_deref(), Some("GH-100"));

        let all = load(&store, &[SCOPE.to_string()], true).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[1].scope, "github:o/z");
        assert_eq!(
            all[1].rows,
            vec![QueueRow {
                ticket: "GH-5".to_string(),
                source: QueueSource::Autopilot,
                state: RowState::Waiting(None),
            }]
        );
    }

    #[test]
    fn move_and_set_epic_write_the_store() {
        let (_dir, store) = store();
        for t in ["GH-1", "GH-2"] {
            store.enqueue_merge(SCOPE, t, QueueSource::Manual).unwrap();
        }
        let moved = change(execute(
            Some(&store),
            None,
            Cmd::MoveQueuedMerge {
                scope: SCOPE.to_string(),
                ticket: "GH-2".to_string(),
                index: 0,
            },
        ));
        assert_eq!(moved.to_string(), "moved GH-2 to #1 in the merge queue");
        assert_eq!(store.merge_queue(SCOPE).unwrap()[0].ticket, "GH-2");

        change(execute(
            Some(&store),
            None,
            Cmd::SetMergeEpic {
                scope: SCOPE.to_string(),
                epic: Some("GH-100".to_string()),
            },
        ));
        assert_eq!(store.merge_epic(SCOPE).unwrap().as_deref(), Some("GH-100"));
    }

    #[test]
    fn without_a_store_every_cmd_reports_an_error() {
        let msgs = execute(
            None,
            None,
            Cmd::LoadMergeQueue {
                scopes: Vec::new(),
                all: false,
            },
        );
        assert!(matches!(msgs.as_slice(), [Msg::MergeQueueLoaded(Err(_))]));
    }
}
