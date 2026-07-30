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
    /// Issue key, e.g. `AX-123`.
    pub key: String,
    /// One-line issue summary.
    pub summary: String,
    /// Current workflow status name.
    pub status: String,
    /// Browsable URL for the issue (`{base_url}/browse/{key}`).
    pub url: String,
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
    /// Tickets currently shown on the board.
    pub tickets: Vec<TicketSummary>,
    /// Index into `tickets` of the currently selected ticket. Always clamped
    /// into bounds (`0` when `tickets` is empty).
    pub selected: usize,
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
        self.tickets.get(self.selected)
    }
}

/// Every event the reducer can react to.
#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    /// Move the current selection/scroll up.
    Up,
    /// Move the current selection/scroll down.
    Down,
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
            app.tickets = tickets;
            clamp_selected(&mut app);
            (app, Vec::new())
        }
        Msg::TicketsFailed(err) => {
            app.status_line = err;
            (app, Vec::new())
        }
        // Enter/Back/transition messages are handled once the detail and
        // transition-menu screens exist; until then they are no-ops.
        Msg::Enter
        | Msg::Back
        | Msg::TransitionsLoaded(_)
        | Msg::TransitionsFailed(_)
        | Msg::TransitionApplied { .. }
        | Msg::TransitionFailed(_) => (app, Vec::new()),
    }
}

/// Move the current selection/scroll up by one, saturating at the top.
fn move_up(app: &mut App) {
    match app.screen {
        Screen::Board => app.selected = app.selected.saturating_sub(1),
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
            if !app.tickets.is_empty() {
                app.selected = (app.selected + 1).min(app.tickets.len() - 1);
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

/// Clamp `selected` into the bounds of `tickets`, resetting to `0` when the
/// list is empty.
fn clamp_selected(app: &mut App) {
    if app.tickets.is_empty() {
        app.selected = 0;
    } else if app.selected >= app.tickets.len() {
        app.selected = app.tickets.len() - 1;
    }
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
        }
    }

    fn board_with(tickets: Vec<TicketSummary>, selected: usize) -> App {
        App {
            tickets,
            selected,
            ..App::new()
        }
    }

    #[test]
    fn up_on_board_is_a_noop_when_empty() {
        let app = board_with(vec![], 0);
        let (app, cmds) = update(app, Msg::Up);
        assert_eq!(app.selected, 0);
        assert!(cmds.is_empty());
    }

    #[test]
    fn down_on_board_is_a_noop_when_empty() {
        let app = board_with(vec![], 0);
        let (app, cmds) = update(app, Msg::Down);
        assert_eq!(app.selected, 0);
        assert!(cmds.is_empty());
    }

    #[test]
    fn up_clamps_at_zero() {
        let app = board_with(vec![ticket("AX-1"), ticket("AX-2")], 0);
        let (app, _) = update(app, Msg::Up);
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn down_clamps_at_last_index() {
        let app = board_with(vec![ticket("AX-1"), ticket("AX-2")], 1);
        let (app, _) = update(app, Msg::Down);
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn up_and_down_move_selection_within_bounds() {
        let app = board_with(vec![ticket("AX-1"), ticket("AX-2"), ticket("AX-3")], 1);
        let (app, _) = update(app, Msg::Down);
        assert_eq!(app.selected, 2);
        let (app, _) = update(app, Msg::Up);
        assert_eq!(app.selected, 1);
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
        let app = board_with(vec![ticket("AX-1"), ticket("AX-2"), ticket("AX-3")], 2);
        let (app, cmds) = update(app, Msg::TicketsLoaded(vec![ticket("AX-9")]));
        assert_eq!(app.tickets, vec![ticket("AX-9")]);
        assert_eq!(app.selected, 0);
        assert!(cmds.is_empty());
    }

    #[test]
    fn tickets_loaded_with_empty_list_resets_selected_to_zero() {
        let app = board_with(vec![ticket("AX-1")], 0);
        let (app, _) = update(app, Msg::TicketsLoaded(vec![]));
        assert_eq!(app.selected, 0);
        assert!(app.tickets.is_empty());
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
        let app = board_with(vec![ticket("AX-1"), ticket("AX-2")], 1);
        let (_, cmds) = update(app, Msg::OpenInBrowser);
        assert_eq!(
            cmds,
            vec![Cmd::OpenUrl(
                "https://example.atlassian.net/browse/AX-2".to_string()
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
}
