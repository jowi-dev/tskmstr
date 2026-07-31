//! Rendering: pure functions from [`App`] state to widgets on a [`Frame`].
//!
//! Nothing here reads events or performs I/O; `crate::tui::event` is the only
//! module that touches a real terminal. That split is what lets rendering be
//! smoke-tested with ratatui's `TestBackend`.
//!
//! The board is always drawn first, one bordered column per status. The
//! detail and transition-menu screens layer centered floating windows on top
//! of it (via [`Clear`]), so the board stays visible behind them.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

use crate::tui::app::{App, Column, Screen};

/// Draw the current screen (and the help overlay, if shown) into `frame`.
pub fn draw(frame: &mut Frame, app: &App) {
    let (body, status_area) = split_body_and_status(frame.area());

    draw_board_columns(frame, app, body);

    match app.screen {
        Screen::Board => {}
        Screen::Detail => draw_detail_window(frame, app),
        Screen::TransitionMenu => {
            draw_detail_window(frame, app);
            draw_transition_window(frame, app);
        }
    }

    draw_status_bar(frame, status_area, &app.status_line, hint_for(app.screen));

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

/// The key-hint text shown in the status bar for `screen`.
fn hint_for(screen: Screen) -> &'static str {
    match screen {
        Screen::Board => "h/l column  j/k move  Enter open  r refresh  o browser  ? help  q quit",
        Screen::Detail => "j/k scroll  Enter transitions  Esc back  ? help  q quit",
        Screen::TransitionMenu => "j/k move  Enter apply  Esc back  ? help  q quit",
    }
}

/// The sprint board: one bordered column per status, laid out left to right
/// with equal widths. The selected column is highlighted, as is the
/// selected ticket within it.
fn draw_board_columns(frame: &mut Frame, app: &App, area: Rect) {
    if app.columns.is_empty() {
        frame.render_widget(Block::default().borders(Borders::ALL).title("Board"), area);
        return;
    }

    let column_count = app.columns.len() as u32;
    let constraints: Vec<Constraint> = (0..column_count)
        .map(|_| Constraint::Ratio(1, column_count))
        .collect();
    let areas = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(area);

    for (index, column) in app.columns.iter().enumerate() {
        let is_selected_column = index == app.selected_col;
        let selected_row = is_selected_column.then_some(app.selected_row);
        draw_column(
            frame,
            areas[index],
            column,
            is_selected_column,
            selected_row,
        );
    }
}

/// One column of the board: a bordered, titled list of its tickets.
fn draw_column(
    frame: &mut Frame,
    area: Rect,
    column: &Column,
    is_selected: bool,
    selected_row: Option<usize>,
) {
    let title = format!("{} ({})", column.title, column.tickets.len());
    let border_style = if is_selected {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };

    let items: Vec<ListItem> = column
        .tickets
        .iter()
        .map(|t| ListItem::new(format!("{}  {}", t.key, t.summary)))
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(border_style),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    let mut state = ListState::default();
    if let Some(row) = selected_row
        && !column.tickets.is_empty()
    {
        state.select(Some(row));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

/// A centered floating window (~80% wide, ~70% tall) showing the selected
/// ticket's full detail, drawn over the board.
fn draw_detail_window(frame: &mut Frame, app: &App) {
    let area = centered_rect(80, 70, frame.area());
    frame.render_widget(Clear, area);

    let title = match app.selected_ticket() {
        Some(ticket) => format!("[{}] {}", ticket.key, ticket.summary),
        None => "Detail".to_string(),
    };

    let text = match app.selected_ticket() {
        Some(ticket) => vec![
            Line::from(vec![Span::styled(
                ticket.status.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            )]),
            Line::from(ticket.url.clone()),
            Line::from(""),
            Line::from(ticket.description.clone()),
        ],
        None => vec![Line::from("No ticket selected")],
    };

    let paragraph = Paragraph::new(text)
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false })
        .scroll((app.detail_scroll, 0));
    frame.render_widget(paragraph, area);
}

/// A smaller centered floating window (~40% wide, ~40% tall), layered on top
/// of the detail window, listing the workflow transitions available on the
/// selected ticket.
fn draw_transition_window(frame: &mut Frame, app: &App) {
    let area = centered_rect(40, 40, frame.area());
    frame.render_widget(Clear, area);

    let items: Vec<ListItem> = app
        .transitions
        .iter()
        .map(|t| ListItem::new(t.name.clone()))
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("Move to"))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    let mut state = ListState::default();
    if !app.transitions.is_empty() {
        state.select(Some(app.transition_selected));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

/// A centered overlay listing every keybinding.
fn draw_help_overlay(frame: &mut Frame) {
    let area = centered_rect(60, 60, frame.area());
    let lines = vec![
        Line::from("j / Down    move down"),
        Line::from("k / Up      move up"),
        Line::from("h / Left    previous column"),
        Line::from("l / Right   next column"),
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
    frame.render_widget(Clear, area);
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

    fn ticket_with(key: &str, status: &str, status_category: &str) -> TicketSummary {
        TicketSummary {
            status: status.to_string(),
            status_category: status_category.to_string(),
            ..ticket(key)
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

    fn three_column_app() -> App {
        App {
            columns: group_into_columns(vec![
                ticket_with("PROJ-1", "To Do", "new"),
                ticket_with("PROJ-2", "In Progress", "indeterminate"),
                ticket_with("PROJ-3", "Done", "done"),
            ]),
            ..App::new()
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
            columns: group_into_columns(vec![ticket("PROJ-1")]),
            status_line: "Refreshing...".to_string(),
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        assert!(text.contains("PROJ-1"));
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
    fn draws_board_with_three_columns_renders_all_column_titles() {
        let app = three_column_app();
        let text = buffer_text(&render(&app));
        assert!(text.contains("To Do (1)"));
        assert!(text.contains("In Progress (1)"));
        assert!(text.contains("Done (1)"));
    }

    #[test]
    fn detail_overlay_leaves_board_column_titles_visible_and_shows_key_title() {
        let app = App {
            screen: Screen::Detail,
            ..three_column_app()
        };
        let text = buffer_text(&render(&app));
        // The board is still visible behind the floating detail window.
        assert!(text.contains("To Do (1)"));
        assert!(text.contains("In Progress (1)"));
        assert!(text.contains("Done (1)"));
        // The detail window itself, titled with the selected ticket's key.
        assert!(text.contains("[PROJ-1] Fix the thing"));
    }

    #[test]
    fn draws_detail_with_ticket_fields_and_description() {
        let app = App {
            columns: group_into_columns(vec![ticket("PROJ-1")]),
            screen: Screen::Detail,
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        assert!(text.contains("[PROJ-1]"));
        assert!(text.contains("A longer description"));
    }

    #[test]
    fn draws_transition_menu_with_transition_names() {
        let app = App {
            columns: group_into_columns(vec![ticket("PROJ-1")]),
            screen: Screen::TransitionMenu,
            transitions: vec![transition("11", "Start Progress"), transition("31", "Done")],
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        assert!(text.contains("Start Progress"));
        assert!(text.contains("Done"));
    }

    #[test]
    fn transition_window_has_move_to_title() {
        let app = App {
            screen: Screen::TransitionMenu,
            transitions: vec![transition("11", "Start Progress")],
            ..three_column_app()
        };
        let text = buffer_text(&render(&app));
        assert!(text.contains("Move to"));
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
            columns: group_into_columns(vec![ticket("PROJ-1")]),
            screen: Screen::Detail,
            show_help: true,
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        assert!(text.contains("Help"));
    }
}
