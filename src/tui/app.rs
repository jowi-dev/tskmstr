//! Pure Elm-style state and reducer for the TUI.
//!
//! [`App`] holds all UI state, [`Msg`] is every event the reducer can react
//! to, and [`update`] is the single place state transitions happen. No I/O
//! occurs here: [`Cmd`] values name the I/O the caller (`crate::tui::event`)
//! should perform next, and *Failed messages let callers feed I/O errors back
//! in without `update` ever needing to panic.

use crate::jira::types::Transition;

/// A ticket as displayed on the board, derived from a
/// [`crate::jira::types::Issue`] plus the configured Jira base URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TicketSummary {
    /// Issue key, e.g. `PROJ-123`.
    pub key: String,
    /// One-line issue summary.
    pub summary: String,
    /// Current workflow status name.
    pub status: String,
    /// Browsable URL for the issue (`{base_url}/browse/{key}`).
    pub url: String,
    /// Plain-text description, extracted from the issue's ADF description
    /// via [`crate::jira::adf::adf_to_text`].
    pub description: String,
    /// Status category key (`new`, `indeterminate`, `done`, or anything else
    /// Jira reports), used to order board columns.
    pub status_category: String,
}

/// One column of the sprint board: all tickets currently in a given status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    /// Status name shared by every ticket in this column.
    pub title: String,
    /// Tickets in this column, in fetch order.
    pub tickets: Vec<TicketSummary>,
}

/// Rank a status category for column ordering: `new` sorts first, then
/// `indeterminate`, then `done`, then anything unrecognized.
fn category_rank(category: &str) -> u8 {
    match category {
        "new" => 0,
        "indeterminate" => 1,
        "done" => 2,
        _ => 3,
    }
}

/// Group `tickets` into [`Column`]s, one per distinct status name.
///
/// Columns are ordered by status category rank (new, then indeterminate,
/// then done, then unknown categories), and alphabetically by status name
/// within a category. Tickets keep their relative fetch order within a
/// column.
pub fn group_into_columns(tickets: Vec<TicketSummary>) -> Vec<Column> {
    let mut columns: Vec<Column> = Vec::new();

    for ticket in tickets {
        match columns.iter_mut().find(|c| c.title == ticket.status) {
            Some(column) => column.tickets.push(ticket),
            None => columns.push(Column {
                title: ticket.status.clone(),
                tickets: vec![ticket],
            }),
        }
    }

    columns.sort_by(|a, b| {
        let rank_a = category_rank(&a.tickets[0].status_category);
        let rank_b = category_rank(&b.tickets[0].status_category);
        rank_a.cmp(&rank_b).then_with(|| a.title.cmp(&b.title))
    });

    columns
}

/// Which screen is currently shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Screen {
    /// The list of open tickets.
    #[default]
    Board,
    /// Full detail view of the ticket selected on the board.
    Detail,
    /// Menu of workflow transitions available on the selected ticket.
    TransitionMenu,
}

/// All state needed to render and drive the TUI.
#[derive(Debug, Clone, Default)]
pub struct App {
    /// Board columns, one per status, in display order.
    pub columns: Vec<Column>,
    /// Index into `columns` of the currently selected column. Always clamped
    /// into bounds (`0` when `columns` is empty).
    pub selected_col: usize,
    /// Index into the selected column's tickets of the currently selected
    /// ticket. Always clamped into bounds (`0` when the column is empty).
    pub selected_row: usize,
    /// The screen currently shown.
    pub screen: Screen,
    /// Transitions available on the selected ticket, populated when
    /// [`Screen::TransitionMenu`] opens.
    pub transitions: Vec<Transition>,
    /// Index into `transitions` of the currently highlighted transition.
    pub transition_selected: usize,
    /// Scroll offset into the detail view's body text.
    pub detail_scroll: u16,
    /// Feedback from the last action or error, shown in the status bar.
    pub status_line: String,
    /// Whether the help overlay is shown.
    pub show_help: bool,
    /// Set when the event loop should exit.
    pub quit: bool,
}

impl App {
    /// An app with no tickets, showing the board.
    pub fn new() -> Self {
        Self::default()
    }

    /// The currently selected ticket, if any.
    pub fn selected_ticket(&self) -> Option<&TicketSummary> {
        self.columns
            .get(self.selected_col)?
            .tickets
            .get(self.selected_row)
    }
}

/// Every event the reducer can react to.
#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    /// Move the current selection/scroll up.
    Up,
    /// Move the current selection/scroll down.
    Down,
    /// Move the selected board column left. Ignored on non-board screens.
    Left,
    /// Move the selected board column right. Ignored on non-board screens.
    Right,
    /// Activate the current selection.
    Enter,
    /// Go back to the previous screen, or quit from the board.
    Back,
    /// Reload the ticket list.
    Refresh,
    /// Open the selected ticket's URL in a browser.
    OpenInBrowser,
    /// Toggle the help overlay.
    ToggleHelp,
    /// Quit the application.
    Quit,
    /// The ticket list finished loading.
    TicketsLoaded(Vec<TicketSummary>),
    /// The ticket list failed to load.
    TicketsFailed(String),
    /// Transitions for the selected ticket finished loading.
    TransitionsLoaded(Vec<Transition>),
    /// Transitions for the selected ticket failed to load.
    TransitionsFailed(String),
    /// A transition was successfully applied.
    TransitionApplied {
        /// Key of the ticket that was transitioned.
        key: String,
        /// New status name after the transition.
        status: String,
        /// New status category key after the transition, used to regroup
        /// the ticket into the right board column.
        status_category: String,
    },
    /// A transition failed to apply.
    TransitionFailed(String),
}

/// I/O the caller should perform as a result of [`update`].
///
/// `update` itself never performs I/O; it only describes what should happen.
/// The caller (`crate::tui::event`) executes each `Cmd` and feeds the
/// resulting `Msg` back through `update`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    /// Fetch the current user's open tickets.
    FetchTickets,
    /// Fetch the workflow transitions available on `key`.
    FetchTransitions {
        /// Ticket key to fetch transitions for.
        key: String,
    },
    /// Apply `transition_id` to `key`.
    ApplyTransition {
        /// Ticket key to transition.
        key: String,
        /// ID of the transition to apply.
        transition_id: String,
    },
    /// Open `url` in the user's default browser.
    OpenUrl(String),
}

/// Advance `app` in response to `msg`, returning the new state and any
/// commands the caller should execute.
///
/// Pure: performs no I/O. All failure-mode messages (`*Failed`) set
/// `status_line` rather than panicking.
pub fn update(mut app: App, msg: Msg) -> (App, Vec<Cmd>) {
    match msg {
        Msg::Up => {
            move_up(&mut app);
            (app, Vec::new())
        }
        Msg::Down => {
            move_down(&mut app);
            (app, Vec::new())
        }
        Msg::Left => {
            move_left(&mut app);
            (app, Vec::new())
        }
        Msg::Right => {
            move_right(&mut app);
            (app, Vec::new())
        }
        Msg::Refresh => {
            app.status_line = "Refreshing...".to_string();
            (app, vec![Cmd::FetchTickets])
        }
        Msg::OpenInBrowser => {
            let cmds = match app.selected_ticket() {
                Some(ticket) => vec![Cmd::OpenUrl(ticket.url.clone())],
                None => Vec::new(),
            };
            (app, cmds)
        }
        Msg::ToggleHelp => {
            app.show_help = !app.show_help;
            (app, Vec::new())
        }
        Msg::Quit => {
            app.quit = true;
            (app, Vec::new())
        }
        Msg::TicketsLoaded(tickets) => {
            let preferred_key = app.selected_ticket().map(|t| t.key.clone());
            app.columns = group_into_columns(tickets);
            reselect(&mut app, preferred_key);
            (app, Vec::new())
        }
        Msg::TicketsFailed(err) => {
            app.status_line = err;
            (app, Vec::new())
        }
        Msg::Enter => enter(app),
        Msg::Back => {
            back(&mut app);
            (app, Vec::new())
        }
        Msg::TransitionsLoaded(transitions) => {
            app.transitions = transitions;
            app.transition_selected = 0;
            app.screen = Screen::TransitionMenu;
            (app, Vec::new())
        }
        Msg::TransitionsFailed(err) => {
            app.status_line = err;
            (app, Vec::new())
        }
        Msg::TransitionApplied {
            key,
            status,
            status_category,
        } => {
            let mut tickets = flatten(&app.columns);
            if let Some(ticket) = tickets.iter_mut().find(|t| t.key == key) {
                ticket.status = status.clone();
                ticket.status_category = status_category;
            }
            app.columns = group_into_columns(tickets);
            reselect(&mut app, Some(key.clone()));
            app.status_line = format!("{key} -> {status}");
            app.screen = Screen::Detail;
            (app, Vec::new())
        }
        Msg::TransitionFailed(err) => {
            app.status_line = err;
            (app, Vec::new())
        }
    }
}

/// Handle [`Msg::Enter`]: activate whatever is selected on the current
/// screen.
fn enter(mut app: App) -> (App, Vec<Cmd>) {
    match app.screen {
        Screen::Board => {
            if app.selected_ticket().is_some() {
                app.screen = Screen::Detail;
            }
            (app, Vec::new())
        }
        Screen::Detail => match app.selected_ticket() {
            Some(ticket) => {
                let key = ticket.key.clone();
                (app, vec![Cmd::FetchTransitions { key }])
            }
            None => (app, Vec::new()),
        },
        Screen::TransitionMenu => {
            let cmd = match (
                app.selected_ticket(),
                app.transitions.get(app.transition_selected),
            ) {
                (Some(ticket), Some(transition)) => Some(Cmd::ApplyTransition {
                    key: ticket.key.clone(),
                    transition_id: transition.id.clone(),
                }),
                _ => None,
            };
            (app, cmd.into_iter().collect())
        }
    }
}

/// Handle [`Msg::Back`]: step back a screen, or quit from the board.
fn back(app: &mut App) {
    match app.screen {
        Screen::Board => app.quit = true,
        Screen::Detail => app.screen = Screen::Board,
        Screen::TransitionMenu => app.screen = Screen::Detail,
    }
}

/// Move the current selection/scroll up by one, saturating at the top.
fn move_up(app: &mut App) {
    match app.screen {
        Screen::Board => app.selected_row = app.selected_row.saturating_sub(1),
        Screen::Detail => app.detail_scroll = app.detail_scroll.saturating_sub(1),
        Screen::TransitionMenu => {
            app.transition_selected = app.transition_selected.saturating_sub(1);
        }
    }
}

/// Move the current selection/scroll down by one, clamping at the bottom of
/// the relevant list. Detail scroll has no known upper bound at this layer,
/// so it is left to increase; the detail view clamps what it displays.
fn move_down(app: &mut App) {
    match app.screen {
        Screen::Board => {
            if let Some(len) = current_column_len(app) {
                app.selected_row = (app.selected_row + 1).min(len - 1);
            }
        }
        Screen::Detail => app.detail_scroll = app.detail_scroll.saturating_add(1),
        Screen::TransitionMenu => {
            if !app.transitions.is_empty() {
                app.transition_selected =
                    (app.transition_selected + 1).min(app.transitions.len() - 1);
            }
        }
    }
}

/// Move the selected board column left by one, saturating at the first
/// column. No-op outside [`Screen::Board`].
fn move_left(app: &mut App) {
    if app.screen != Screen::Board || app.columns.is_empty() {
        return;
    }
    app.selected_col = app.selected_col.saturating_sub(1);
    clamp_row(app);
}

/// Move the selected board column right by one, clamping at the last
/// column. No-op outside [`Screen::Board`].
fn move_right(app: &mut App) {
    if app.screen != Screen::Board || app.columns.is_empty() {
        return;
    }
    app.selected_col = (app.selected_col + 1).min(app.columns.len() - 1);
    clamp_row(app);
}

/// The number of tickets in the currently selected column, or `None` if
/// there are no columns.
fn current_column_len(app: &App) -> Option<usize> {
    app.columns.get(app.selected_col).map(|c| c.tickets.len())
}

/// Clamp `selected_row` into the bounds of the currently selected column,
/// resetting to `0` when that column is empty.
fn clamp_row(app: &mut App) {
    match current_column_len(app) {
        Some(0) | None => app.selected_row = 0,
        Some(len) if app.selected_row >= len => app.selected_row = len - 1,
        Some(_) => {}
    }
}

/// Flatten every column's tickets back into a single list, in column then
/// fetch order.
fn flatten(columns: &[Column]) -> Vec<TicketSummary> {
    columns.iter().flat_map(|c| c.tickets.clone()).collect()
}

/// Select the ticket with key `key`, if it exists in `app.columns`. Returns
/// whether it was found.
fn select_by_key(app: &mut App, key: &str) -> bool {
    for (col_index, column) in app.columns.iter().enumerate() {
        if let Some(row_index) = column.tickets.iter().position(|t| t.key == key) {
            app.selected_col = col_index;
            app.selected_row = row_index;
            return true;
        }
    }
    false
}

/// Re-establish selection after `app.columns` has been rebuilt: prefer
/// keeping `preferred_key` selected if it still exists, otherwise clamp the
/// existing indices into the new bounds.
fn reselect(app: &mut App, preferred_key: Option<String>) {
    let found = preferred_key.is_some_and(|key| select_by_key(app, &key));
    if !found {
        clamp_selection(app);
    }
}

/// Clamp `selected_col`/`selected_row` into the bounds of `columns`,
/// resetting both to `0` when `columns` is empty.
fn clamp_selection(app: &mut App) {
    if app.columns.is_empty() {
        app.selected_col = 0;
        app.selected_row = 0;
        return;
    }
    if app.selected_col >= app.columns.len() {
        app.selected_col = app.columns.len() - 1;
    }
    clamp_row(app);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket(key: &str) -> TicketSummary {
        TicketSummary {
            key: key.to_string(),
            summary: format!("Summary for {key}"),
            status: "To Do".to_string(),
            url: format!("https://example.atlassian.net/browse/{key}"),
            description: format!("Description for {key}"),
            status_category: "new".to_string(),
        }
    }

    fn ticket_with(key: &str, status: &str, status_category: &str) -> TicketSummary {
        TicketSummary {
            status: status.to_string(),
            status_category: status_category.to_string(),
            ..ticket(key)
        }
    }

    fn board_with(tickets: Vec<TicketSummary>, selected_row: usize) -> App {
        App {
            columns: group_into_columns(tickets),
            selected_col: 0,
            selected_row,
            ..App::new()
        }
    }

    #[test]
    fn up_on_board_is_a_noop_when_empty() {
        let app = board_with(vec![], 0);
        let (app, cmds) = update(app, Msg::Up);
        assert_eq!(app.selected_row, 0);
        assert!(cmds.is_empty());
    }

    #[test]
    fn down_on_board_is_a_noop_when_empty() {
        let app = board_with(vec![], 0);
        let (app, cmds) = update(app, Msg::Down);
        assert_eq!(app.selected_row, 0);
        assert!(cmds.is_empty());
    }

    #[test]
    fn up_clamps_at_zero() {
        let app = board_with(vec![ticket("PROJ-1"), ticket("PROJ-2")], 0);
        let (app, _) = update(app, Msg::Up);
        assert_eq!(app.selected_row, 0);
    }

    #[test]
    fn down_clamps_at_last_index() {
        let app = board_with(vec![ticket("PROJ-1"), ticket("PROJ-2")], 1);
        let (app, _) = update(app, Msg::Down);
        assert_eq!(app.selected_row, 1);
    }

    #[test]
    fn up_and_down_move_selection_within_bounds() {
        let app = board_with(
            vec![ticket("PROJ-1"), ticket("PROJ-2"), ticket("PROJ-3")],
            1,
        );
        let (app, _) = update(app, Msg::Down);
        assert_eq!(app.selected_row, 2);
        let (app, _) = update(app, Msg::Up);
        assert_eq!(app.selected_row, 1);
    }

    #[test]
    fn left_and_right_are_noops_when_board_has_one_column() {
        let app = board_with(vec![ticket("PROJ-1"), ticket("PROJ-2")], 1);
        let (app, _) = update(app, Msg::Right);
        assert_eq!(app.selected_col, 0);
        assert_eq!(app.selected_row, 1);
        let (app, _) = update(app, Msg::Left);
        assert_eq!(app.selected_col, 0);
        assert_eq!(app.selected_row, 1);
    }

    #[test]
    fn left_and_right_move_between_columns_and_clamp() {
        let tickets = vec![
            ticket_with("PROJ-1", "To Do", "new"),
            ticket_with("PROJ-2", "In Progress", "indeterminate"),
            ticket_with("PROJ-3", "In Progress", "indeterminate"),
            ticket_with("PROJ-4", "Done", "done"),
        ];
        let app = App {
            columns: group_into_columns(tickets),
            selected_col: 0,
            selected_row: 0,
            ..App::new()
        };

        // To Do (1 ticket) -> In Progress (2 tickets): row stays 0.
        let (app, _) = update(app, Msg::Right);
        assert_eq!(app.selected_col, 1);
        assert_eq!(app.selected_row, 0);

        // Move to the second ticket in In Progress, then right into Done
        // (1 ticket): row must clamp down to 0.
        let (app, _) = update(app, Msg::Down);
        assert_eq!(app.selected_row, 1);
        let (app, _) = update(app, Msg::Right);
        assert_eq!(app.selected_col, 2);
        assert_eq!(app.selected_row, 0);

        // Right again clamps at the last column.
        let (app, _) = update(app, Msg::Right);
        assert_eq!(app.selected_col, 2);

        // Left steps back through columns.
        let (app, _) = update(app, Msg::Left);
        assert_eq!(app.selected_col, 1);
        let (app, _) = update(app, Msg::Left);
        assert_eq!(app.selected_col, 0);
        let (app, _) = update(app, Msg::Left);
        assert_eq!(app.selected_col, 0);
    }

    #[test]
    fn left_and_right_are_noops_when_board_is_empty() {
        let app = board_with(vec![], 0);
        let (app, cmds) = update(app, Msg::Right);
        assert_eq!(app.selected_col, 0);
        assert!(cmds.is_empty());
        let (app, cmds) = update(app, Msg::Left);
        assert_eq!(app.selected_col, 0);
        assert!(cmds.is_empty());
    }

    #[test]
    fn left_and_right_are_ignored_off_the_board_screen() {
        let app = App {
            screen: Screen::Detail,
            detail_scroll: 3,
            ..board_with(vec![ticket("PROJ-1")], 0)
        };
        let (app, _) = update(app, Msg::Right);
        assert_eq!(app.selected_col, 0);
        assert_eq!(app.detail_scroll, 3);
    }

    #[test]
    fn refresh_sets_status_line_and_emits_fetch_tickets() {
        let app = App::new();
        let (app, cmds) = update(app, Msg::Refresh);
        assert_eq!(app.status_line, "Refreshing...");
        assert_eq!(cmds, vec![Cmd::FetchTickets]);
    }

    #[test]
    fn tickets_loaded_replaces_list_and_clamps_selected() {
        let app = board_with(
            vec![ticket("PROJ-1"), ticket("PROJ-2"), ticket("PROJ-3")],
            2,
        );
        let (app, cmds) = update(app, Msg::TicketsLoaded(vec![ticket("PROJ-9")]));
        assert_eq!(
            app.columns,
            vec![Column {
                title: "To Do".to_string(),
                tickets: vec![ticket("PROJ-9")],
            }]
        );
        assert_eq!(app.selected_col, 0);
        assert_eq!(app.selected_row, 0);
        assert!(cmds.is_empty());
    }

    #[test]
    fn tickets_loaded_with_empty_list_resets_selected_to_zero() {
        let app = board_with(vec![ticket("PROJ-1")], 0);
        let (app, _) = update(app, Msg::TicketsLoaded(vec![]));
        assert_eq!(app.selected_col, 0);
        assert_eq!(app.selected_row, 0);
        assert!(app.columns.is_empty());
    }

    #[test]
    fn tickets_loaded_preserves_selection_by_key_when_still_present() {
        let app = board_with(vec![ticket("PROJ-1"), ticket("PROJ-2")], 1);
        let (app, _) = update(
            app,
            Msg::TicketsLoaded(vec![ticket("PROJ-0"), ticket("PROJ-2"), ticket("PROJ-3")]),
        );
        assert_eq!(app.selected_ticket().unwrap().key, "PROJ-2");
    }

    #[test]
    fn tickets_failed_sets_status_line() {
        let app = App::new();
        let (app, cmds) = update(app, Msg::TicketsFailed("boom".to_string()));
        assert_eq!(app.status_line, "boom");
        assert!(cmds.is_empty());
    }

    #[test]
    fn quit_sets_quit_flag() {
        let app = App::new();
        let (app, cmds) = update(app, Msg::Quit);
        assert!(app.quit);
        assert!(cmds.is_empty());
    }

    #[test]
    fn open_in_browser_emits_open_url_for_selected_ticket() {
        let app = board_with(vec![ticket("PROJ-1"), ticket("PROJ-2")], 1);
        let (_, cmds) = update(app, Msg::OpenInBrowser);
        assert_eq!(
            cmds,
            vec![Cmd::OpenUrl(
                "https://example.atlassian.net/browse/PROJ-2".to_string()
            )]
        );
    }

    #[test]
    fn open_in_browser_with_no_tickets_emits_nothing() {
        let app = board_with(vec![], 0);
        let (_, cmds) = update(app, Msg::OpenInBrowser);
        assert!(cmds.is_empty());
    }

    #[test]
    fn toggle_help_flips_show_help() {
        let app = App::new();
        let (app, _) = update(app, Msg::ToggleHelp);
        assert!(app.show_help);
        let (app, _) = update(app, Msg::ToggleHelp);
        assert!(!app.show_help);
    }

    fn transition(id: &str, name: &str) -> Transition {
        use crate::jira::types::{Status, StatusCategory};

        Transition {
            id: id.to_string(),
            name: name.to_string(),
            to: Status {
                name: name.to_string(),
                status_category: StatusCategory {
                    key: "indeterminate".to_string(),
                },
            },
        }
    }

    #[test]
    fn enter_on_board_with_selection_opens_detail() {
        let app = board_with(vec![ticket("PROJ-1")], 0);
        let (app, cmds) = update(app, Msg::Enter);
        assert_eq!(app.screen, Screen::Detail);
        assert!(cmds.is_empty());
    }

    #[test]
    fn enter_on_board_with_no_tickets_stays_on_board() {
        let app = board_with(vec![], 0);
        let (app, cmds) = update(app, Msg::Enter);
        assert_eq!(app.screen, Screen::Board);
        assert!(cmds.is_empty());
    }

    #[test]
    fn enter_on_detail_emits_fetch_transitions_and_stays_on_detail() {
        let app = App {
            screen: Screen::Detail,
            ..board_with(vec![ticket("PROJ-1")], 0)
        };
        let (app, cmds) = update(app, Msg::Enter);
        assert_eq!(app.screen, Screen::Detail);
        assert_eq!(
            cmds,
            vec![Cmd::FetchTransitions {
                key: "PROJ-1".to_string()
            }]
        );
    }

    #[test]
    fn transitions_loaded_moves_to_transition_menu() {
        let app = App {
            screen: Screen::Detail,
            ..board_with(vec![ticket("PROJ-1")], 0)
        };
        let (app, cmds) = update(
            app,
            Msg::TransitionsLoaded(vec![transition("11", "Start Progress")]),
        );
        assert_eq!(app.screen, Screen::TransitionMenu);
        assert_eq!(app.transitions, vec![transition("11", "Start Progress")]);
        assert_eq!(app.transition_selected, 0);
        assert!(cmds.is_empty());
    }

    #[test]
    fn transitions_failed_sets_status_line_and_stays_on_detail() {
        let app = App {
            screen: Screen::Detail,
            ..board_with(vec![ticket("PROJ-1")], 0)
        };
        let (app, cmds) = update(app, Msg::TransitionsFailed("boom".to_string()));
        assert_eq!(app.status_line, "boom");
        assert_eq!(app.screen, Screen::Detail);
        assert!(cmds.is_empty());
    }

    #[test]
    fn enter_on_transition_menu_emits_apply_transition() {
        let app = App {
            screen: Screen::TransitionMenu,
            transitions: vec![transition("11", "Start Progress")],
            transition_selected: 0,
            ..board_with(vec![ticket("PROJ-1")], 0)
        };
        let (_, cmds) = update(app, Msg::Enter);
        assert_eq!(
            cmds,
            vec![Cmd::ApplyTransition {
                key: "PROJ-1".to_string(),
                transition_id: "11".to_string()
            }]
        );
    }

    #[test]
    fn transition_applied_updates_ticket_status_and_returns_to_detail() {
        let app = App {
            screen: Screen::TransitionMenu,
            transitions: vec![transition("11", "In Progress")],
            ..board_with(vec![ticket("PROJ-1"), ticket("PROJ-2")], 0)
        };
        let (app, cmds) = update(
            app,
            Msg::TransitionApplied {
                key: "PROJ-1".to_string(),
                status: "In Progress".to_string(),
                status_category: "indeterminate".to_string(),
            },
        );
        assert_eq!(app.screen, Screen::Detail);
        assert_eq!(app.status_line, "PROJ-1 -> In Progress");
        assert_eq!(
            flatten(&app.columns)
                .iter()
                .find(|t| t.key == "PROJ-1")
                .unwrap()
                .status,
            "In Progress"
        );
        assert!(cmds.is_empty());
    }

    #[test]
    fn transition_applied_moves_ticket_across_columns_and_selection_follows_it() {
        let tickets = vec![
            ticket_with("PROJ-1", "To Do", "new"),
            ticket_with("PROJ-2", "To Do", "new"),
        ];
        let app = App {
            screen: Screen::TransitionMenu,
            transitions: vec![transition("11", "Done")],
            columns: group_into_columns(tickets),
            selected_col: 0,
            selected_row: 0,
            ..App::new()
        };

        let (app, _) = update(
            app,
            Msg::TransitionApplied {
                key: "PROJ-1".to_string(),
                status: "Done".to_string(),
                status_category: "done".to_string(),
            },
        );

        // PROJ-1 leaves the "To Do" column (now down to just PROJ-2) and lands
        // in a new "Done" column, ordered after "To Do" (new < done).
        assert_eq!(
            app.columns,
            vec![
                Column {
                    title: "To Do".to_string(),
                    tickets: vec![ticket_with("PROJ-2", "To Do", "new")],
                },
                Column {
                    title: "Done".to_string(),
                    tickets: vec![ticket_with("PROJ-1", "Done", "done")],
                },
            ]
        );
        // Selection follows PROJ-1 into its new column.
        assert_eq!(app.selected_col, 1);
        assert_eq!(app.selected_row, 0);
        assert_eq!(app.selected_ticket().unwrap().key, "PROJ-1");
    }

    #[test]
    fn transition_failed_sets_status_line() {
        let app = App {
            screen: Screen::TransitionMenu,
            ..board_with(vec![ticket("PROJ-1")], 0)
        };
        let (app, cmds) = update(app, Msg::TransitionFailed("nope".to_string()));
        assert_eq!(app.status_line, "nope");
        assert!(cmds.is_empty());
    }

    #[test]
    fn back_on_transition_menu_returns_to_detail() {
        let app = App {
            screen: Screen::TransitionMenu,
            ..board_with(vec![ticket("PROJ-1")], 0)
        };
        let (app, _) = update(app, Msg::Back);
        assert_eq!(app.screen, Screen::Detail);
    }

    #[test]
    fn back_on_detail_returns_to_board() {
        let app = App {
            screen: Screen::Detail,
            ..board_with(vec![ticket("PROJ-1")], 0)
        };
        let (app, _) = update(app, Msg::Back);
        assert_eq!(app.screen, Screen::Board);
    }

    #[test]
    fn back_on_board_quits() {
        let app = board_with(vec![ticket("PROJ-1")], 0);
        let (app, _) = update(app, Msg::Back);
        assert!(app.quit);
    }

    #[test]
    fn up_on_detail_scrolls_up_clamped_at_zero() {
        let app = App {
            screen: Screen::Detail,
            detail_scroll: 0,
            ..App::new()
        };
        let (app, _) = update(app, Msg::Up);
        assert_eq!(app.detail_scroll, 0);
    }

    #[test]
    fn down_on_detail_scrolls_down() {
        let app = App {
            screen: Screen::Detail,
            detail_scroll: 2,
            ..App::new()
        };
        let (app, _) = update(app, Msg::Down);
        assert_eq!(app.detail_scroll, 3);
    }

    #[test]
    fn up_and_down_on_transition_menu_move_selection_clamped() {
        let app = App {
            screen: Screen::TransitionMenu,
            transitions: vec![transition("11", "A"), transition("21", "B")],
            transition_selected: 0,
            ..App::new()
        };
        let (app, _) = update(app, Msg::Up);
        assert_eq!(app.transition_selected, 0);
        let (app, _) = update(app, Msg::Down);
        assert_eq!(app.transition_selected, 1);
        let (app, _) = update(app, Msg::Down);
        assert_eq!(app.transition_selected, 1);
    }

    #[test]
    fn group_into_columns_orders_by_category_then_name() {
        struct Case {
            name: &'static str,
            tickets: Vec<TicketSummary>,
            expected: Vec<(&'static str, Vec<&'static str>)>,
        }

        let cases = vec![
            Case {
                name: "empty input produces no columns",
                tickets: vec![],
                expected: vec![],
            },
            Case {
                name: "single status produces a single column",
                tickets: vec![ticket_with("PROJ-1", "To Do", "new")],
                expected: vec![("To Do", vec!["PROJ-1"])],
            },
            Case {
                name: "categories ordered new < indeterminate < done",
                tickets: vec![
                    ticket_with("PROJ-1", "Done", "done"),
                    ticket_with("PROJ-2", "In Progress", "indeterminate"),
                    ticket_with("PROJ-3", "To Do", "new"),
                ],
                expected: vec![
                    ("To Do", vec!["PROJ-3"]),
                    ("In Progress", vec!["PROJ-2"]),
                    ("Done", vec!["PROJ-1"]),
                ],
            },
            Case {
                name: "same category sorts alphabetically by status name",
                tickets: vec![
                    ticket_with("PROJ-1", "In Review", "indeterminate"),
                    ticket_with("PROJ-2", "In Progress", "indeterminate"),
                ],
                expected: vec![
                    ("In Progress", vec!["PROJ-2"]),
                    ("In Review", vec!["PROJ-1"]),
                ],
            },
            Case {
                name: "unknown category sorts after done",
                tickets: vec![
                    ticket_with("PROJ-1", "Done", "done"),
                    ticket_with("PROJ-2", "Weird", "some-unknown-category"),
                ],
                expected: vec![("Done", vec!["PROJ-1"]), ("Weird", vec!["PROJ-2"])],
            },
            Case {
                name: "ticket order within a column is preserved",
                tickets: vec![
                    ticket_with("PROJ-1", "To Do", "new"),
                    ticket_with("PROJ-2", "To Do", "new"),
                    ticket_with("PROJ-3", "To Do", "new"),
                ],
                expected: vec![("To Do", vec!["PROJ-1", "PROJ-2", "PROJ-3"])],
            },
        ];

        for case in cases {
            let columns = group_into_columns(case.tickets);
            let actual: Vec<(&str, Vec<&str>)> = columns
                .iter()
                .map(|c| {
                    (
                        c.title.as_str(),
                        c.tickets.iter().map(|t| t.key.as_str()).collect(),
                    )
                })
                .collect();
            assert_eq!(actual, case.expected, "case: {}", case.name);
        }
    }
}
