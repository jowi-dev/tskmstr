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
    /// Plain-text description, extracted from the issue's ADF description
    /// via [`crate::jira::adf::adf_to_text`].
    pub description: String,
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
        Msg::TransitionApplied { key, status } => {
            if let Some(ticket) = app.tickets.iter_mut().find(|t| t.key == key) {
                ticket.status = status.clone();
            }
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
            description: format!("Description for {key}"),
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
        let app = board_with(vec![ticket("AX-1")], 0);
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
            ..board_with(vec![ticket("AX-1")], 0)
        };
        let (app, cmds) = update(app, Msg::Enter);
        assert_eq!(app.screen, Screen::Detail);
        assert_eq!(
            cmds,
            vec![Cmd::FetchTransitions {
                key: "AX-1".to_string()
            }]
        );
    }

    #[test]
    fn transitions_loaded_moves_to_transition_menu() {
        let app = App {
            screen: Screen::Detail,
            ..board_with(vec![ticket("AX-1")], 0)
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
            ..board_with(vec![ticket("AX-1")], 0)
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
            ..board_with(vec![ticket("AX-1")], 0)
        };
        let (_, cmds) = update(app, Msg::Enter);
        assert_eq!(
            cmds,
            vec![Cmd::ApplyTransition {
                key: "AX-1".to_string(),
                transition_id: "11".to_string()
            }]
        );
    }

    #[test]
    fn transition_applied_updates_ticket_status_and_returns_to_detail() {
        let app = App {
            screen: Screen::TransitionMenu,
            transitions: vec![transition("11", "In Progress")],
            ..board_with(vec![ticket("AX-1"), ticket("AX-2")], 0)
        };
        let (app, cmds) = update(
            app,
            Msg::TransitionApplied {
                key: "AX-1".to_string(),
                status: "In Progress".to_string(),
            },
        );
        assert_eq!(app.screen, Screen::Detail);
        assert_eq!(app.status_line, "AX-1 -> In Progress");
        assert_eq!(
            app.tickets.iter().find(|t| t.key == "AX-1").unwrap().status,
            "In Progress"
        );
        assert!(cmds.is_empty());
    }

    #[test]
    fn transition_failed_sets_status_line() {
        let app = App {
            screen: Screen::TransitionMenu,
            ..board_with(vec![ticket("AX-1")], 0)
        };
        let (app, cmds) = update(app, Msg::TransitionFailed("nope".to_string()));
        assert_eq!(app.status_line, "nope");
        assert!(cmds.is_empty());
    }

    #[test]
    fn back_on_transition_menu_returns_to_detail() {
        let app = App {
            screen: Screen::TransitionMenu,
            ..board_with(vec![ticket("AX-1")], 0)
        };
        let (app, _) = update(app, Msg::Back);
        assert_eq!(app.screen, Screen::Detail);
    }

    #[test]
    fn back_on_detail_returns_to_board() {
        let app = App {
            screen: Screen::Detail,
            ..board_with(vec![ticket("AX-1")], 0)
        };
        let (app, _) = update(app, Msg::Back);
        assert_eq!(app.screen, Screen::Board);
    }

    #[test]
    fn back_on_board_quits() {
        let app = board_with(vec![ticket("AX-1")], 0);
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
}
