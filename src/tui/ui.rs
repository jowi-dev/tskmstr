//! Rendering: pure functions from [`App`] state to widgets on a [`Frame`].
//!
//! Nothing here reads events or performs I/O; `crate::tui::event` is the only
//! module that touches a real terminal. That split is what lets rendering be
//! smoke-tested with ratatui's `TestBackend`.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use crate::tui::app::{App, Screen};

/// Draw the current screen (and the help overlay, if shown) into `frame`.
pub fn draw(frame: &mut Frame, app: &App) {
    match app.screen {
        Screen::Board => draw_board(frame, app),
        Screen::Detail => draw_detail(frame, app),
        Screen::TransitionMenu => draw_transition_menu(frame, app),
    }

    if app.show_help {
        draw_help_overlay(frame);
    }
}

/// Split `area` into a main content region and a one-line bottom status bar.
fn split_body_and_status(area: Rect) -> (Rect, Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    (chunks[0], chunks[1])
}

/// Render the status bar: `status_line` on the left, key hints on the right
/// (hints are dropped if there isn't room for both).
fn draw_status_bar(frame: &mut Frame, area: Rect, status_line: &str, hints: &str) {
    let text = if status_line.is_empty() {
        hints.to_string()
    } else {
        format!("{status_line}  |  {hints}")
    };
    frame.render_widget(Paragraph::new(text), area);
}

/// The flat list index of `app`'s selected ticket across all columns
/// concatenated in order, or `None` if there is no selection.
///
/// Temporary bridge for the still-flat-list board rendering; superseded by
/// per-column rendering.
fn flat_selected_index(app: &App) -> Option<usize> {
    if app.columns.is_empty() {
        return None;
    }
    let before: usize = app.columns[..app.selected_col]
        .iter()
        .map(|c| c.tickets.len())
        .sum();
    Some(before + app.selected_row)
}

/// The board screen: a list of open tickets plus a status bar.
fn draw_board(frame: &mut Frame, app: &App) {
    let (body, status) = split_body_and_status(frame.area());

    let items: Vec<ListItem> = app
        .columns
        .iter()
        .flat_map(|c| c.tickets.iter())
        .map(|t| ListItem::new(format!("{:<10} {:<14} {}", t.key, t.status, t.summary)))
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("Board"))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    let mut state = ListState::default();
    if let Some(flat_index) = flat_selected_index(app) {
        state.select(Some(flat_index));
    }
    frame.render_stateful_widget(list, body, &mut state);

    draw_status_bar(
        frame,
        status,
        &app.status_line,
        "j/k move  Enter open  r refresh  o browser  ? help  q quit",
    );
}

/// The detail screen: the selected ticket's fields plus its scrollable
/// description body.
fn draw_detail(frame: &mut Frame, app: &App) {
    let (body, status) = split_body_and_status(frame.area());

    let text = match app.selected_ticket() {
        Some(ticket) => vec![
            Line::from(vec![
                Span::styled(
                    format!("{} ", ticket.key),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(ticket.status.clone()),
            ]),
            Line::from(ticket.summary.clone()),
            Line::from(ticket.url.clone()),
            Line::from(""),
            Line::from(ticket.description.clone()),
        ],
        None => vec![Line::from("No ticket selected")],
    };

    let paragraph = Paragraph::new(text)
        .block(Block::default().borders(Borders::ALL).title("Detail"))
        .wrap(Wrap { trim: false })
        .scroll((app.detail_scroll, 0));
    frame.render_widget(paragraph, body);

    draw_status_bar(
        frame,
        status,
        &app.status_line,
        "j/k scroll  Enter transitions  Esc back  ? help  q quit",
    );
}

/// The transition menu: the list of workflow transitions available on the
/// selected ticket.
fn draw_transition_menu(frame: &mut Frame, app: &App) {
    let (body, status) = split_body_and_status(frame.area());

    let items: Vec<ListItem> = app
        .transitions
        .iter()
        .map(|t| ListItem::new(t.name.clone()))
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("Transitions"))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    let mut state = ListState::default();
    if !app.transitions.is_empty() {
        state.select(Some(app.transition_selected));
    }
    frame.render_stateful_widget(list, body, &mut state);

    draw_status_bar(
        frame,
        status,
        &app.status_line,
        "j/k move  Enter apply  Esc back  ? help  q quit",
    );
}

/// A centered overlay listing every keybinding.
fn draw_help_overlay(frame: &mut Frame) {
    let area = centered_rect(60, 60, frame.area());
    let lines = vec![
        Line::from("j / Down    move down"),
        Line::from("k / Up      move up"),
        Line::from("Enter       open / apply"),
        Line::from("Esc / q     back (quits from the board)"),
        Line::from("r           refresh tickets"),
        Line::from("o           open in browser"),
        Line::from("?           toggle this help"),
        Line::from(""),
        Line::from("press any key to close"),
    ];
    let paragraph =
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("Help"));
    frame.render_widget(ratatui::widgets::Clear, area);
    frame.render_widget(paragraph, area);
}

/// A `Rect` centered within `area`, `percent_x`/`percent_y` percent of its
/// width/height.
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jira::types::{Status, StatusCategory, Transition};
    use crate::tui::app::{TicketSummary, group_into_columns};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn ticket(key: &str) -> TicketSummary {
        TicketSummary {
            key: key.to_string(),
            summary: "Fix the thing".to_string(),
            status: "In Progress".to_string(),
            url: format!("https://example.atlassian.net/browse/{key}"),
            description: "A longer description of the ticket.".to_string(),
            status_category: "indeterminate".to_string(),
        }
    }

    fn transition(id: &str, name: &str) -> Transition {
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

    fn render(app: &App) -> ratatui::buffer::Buffer {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal should build");
        terminal
            .draw(|frame| draw(frame, app))
            .expect("draw should not panic");
        terminal.backend().buffer().clone()
    }

    fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
        let mut out = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn draws_board_with_ticket_row_and_status_bar() {
        let app = App {
            columns: group_into_columns(vec![ticket("AX-1")]),
            status_line: "Refreshing...".to_string(),
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        assert!(text.contains("AX-1"));
        assert!(text.contains("In Progress"));
        assert!(text.contains("Fix the thing"));
        assert!(text.contains("Refreshing..."));
    }

    #[test]
    fn draws_board_empty_list_without_panicking() {
        let app = App::new();
        let text = buffer_text(&render(&app));
        assert!(text.contains("Board"));
    }

    #[test]
    fn draws_detail_with_ticket_fields_and_description() {
        let app = App {
            columns: group_into_columns(vec![ticket("AX-1")]),
            screen: Screen::Detail,
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        assert!(text.contains("AX-1"));
        assert!(text.contains("A longer description"));
    }

    #[test]
    fn draws_transition_menu_with_transition_names() {
        let app = App {
            columns: group_into_columns(vec![ticket("AX-1")]),
            screen: Screen::TransitionMenu,
            transitions: vec![transition("11", "Start Progress"), transition("31", "Done")],
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        assert!(text.contains("Start Progress"));
        assert!(text.contains("Done"));
    }

    #[test]
    fn draws_help_overlay_on_top_of_board() {
        let app = App {
            show_help: true,
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        assert!(text.contains("Help"));
        assert!(text.contains("toggle this help"));
    }

    #[test]
    fn draws_help_overlay_on_top_of_detail() {
        let app = App {
            columns: group_into_columns(vec![ticket("AX-1")]),
            screen: Screen::Detail,
            show_help: true,
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        assert!(text.contains("Help"));
    }
}
