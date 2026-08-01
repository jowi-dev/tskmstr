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
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap,
};

use crate::tui::app::{App, AssigneeFilter, Column, Screen, TicketSummary};

/// Maximum number of wrapped summary lines shown on a single ticket card.
/// Longer summaries are truncated with a trailing ellipsis so that one long
/// ticket cannot push the rest of the column out of view. Combined with the
/// card's top/bottom border, this caps a single card at 5 rows tall.
const MAX_SUMMARY_LINES: usize = 3;

/// Border rows (top + bottom) added to every card in addition to its wrapped
/// summary lines.
const CARD_BORDER_ROWS: u16 = 2;

/// Draw the current screen (and the help overlay, if shown) into `frame`.
pub fn draw(frame: &mut Frame, app: &App) {
    let (body, status_area) = split_body_and_status(frame.area());

    if app.screen == Screen::Rank {
        draw_rank_list(frame, app, body);
    } else {
        draw_board_columns(frame, app, body);
    }

    match app.screen {
        Screen::Board | Screen::Rank => {}
        Screen::Detail => draw_detail_window(frame, app),
        Screen::TransitionMenu => {
            draw_detail_window(frame, app);
            draw_transition_window(frame, app);
        }
    }

    draw_status_bar(
        frame,
        status_area,
        &status_line_text(app),
        hint_for(app.screen),
    );

    if app.show_help {
        draw_help_overlay(frame);
    }

    if app.show_filter_picker {
        draw_filter_picker(frame, app);
    }
}

/// The status bar's left-hand text: the active assignee filter (when it
/// isn't `Me`) prefixed onto `app.status_line`.
fn status_line_text(app: &App) -> String {
    if app.screen == Screen::Rank || app.filter == AssigneeFilter::Me {
        return app.status_line.clone();
    }
    let filter_text = format!("Filter: {}", app.filter.label());
    if app.status_line.is_empty() {
        filter_text
    } else {
        format!("{filter_text}  |  {}", app.status_line)
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
        Screen::Board => {
            "h/l column  j/k move  Enter open  r refresh  o browser  f filter  ? help  q quit"
        }
        Screen::Detail => "j/k scroll  Enter transitions  Esc back  ? help  q quit",
        Screen::TransitionMenu => "j/k move  Enter apply  Esc back  ? help  q quit",
        Screen::Rank => {
            "j/k move  Enter/Space grab-drop  r refresh  o browser  Esc back  ? help  q quit"
        }
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

    let show_assignee = app.filter != AssigneeFilter::Me;

    for (index, column) in app.columns.iter().enumerate() {
        let is_selected_column = index == app.selected_col;
        let selected_row = is_selected_column.then_some(app.selected_row);
        draw_column(
            frame,
            areas[index],
            column,
            is_selected_column,
            selected_row,
            show_assignee,
        );
    }
}

/// One column of the board: a bordered, titled stack of ticket cards.
///
/// Each ticket renders as its own rounded-border card (see [`card_height`]
/// and [`wrapped_summary`]) rather than a single `List` row, since summaries
/// need to wrap across multiple lines. Because cards are variable height,
/// the column can't rely on `ratatui`'s `List`/`ListState` for scrolling; the
/// visible range of cards is instead recomputed on every frame by
/// [`visible_card_range`] so the selected card always stays fully in view.
fn draw_column(
    frame: &mut Frame,
    area: Rect,
    column: &Column,
    is_selected: bool,
    selected_row: Option<usize>,
    show_assignee: bool,
) {
    let title = format!("{} ({})", column.title, column.tickets.len());
    let border_style = if is_selected {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(border_style);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if column.tickets.is_empty() || inner.width == 0 || inner.height == 0 {
        return;
    }

    let content_width = inner.width.saturating_sub(2).max(1) as usize;
    let heights: Vec<u16> = column
        .tickets
        .iter()
        .map(|t| card_height(t, content_width, show_assignee))
        .collect();

    let selected = selected_row.unwrap_or(0).min(column.tickets.len() - 1);
    let range = visible_card_range(&heights, selected, inner.height);

    let constraints: Vec<Constraint> = heights[range.clone()]
        .iter()
        .map(|h| Constraint::Length(*h))
        .collect();
    let card_areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    for (offset, ticket_index) in range.enumerate() {
        let ticket = &column.tickets[ticket_index];
        let is_selected_card = selected_row == Some(ticket_index);
        draw_card(
            frame,
            card_areas[offset],
            ticket,
            is_selected_card,
            content_width,
            show_assignee,
        );
    }
}

/// One ticket's card: a rounded-border block titled with the issue key,
/// containing its word-wrapped summary (capped at [`MAX_SUMMARY_LINES`]
/// lines). The selected card is rendered reversed so it's clearly
/// distinguished from its neighbors.
fn draw_card(
    frame: &mut Frame,
    area: Rect,
    ticket: &TicketSummary,
    is_selected: bool,
    content_width: usize,
    show_assignee: bool,
) {
    let style = if is_selected {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
    };

    let block = Block::default()
        .border_type(BorderType::Rounded)
        .borders(Borders::ALL)
        .title(ticket.key.clone())
        .border_style(style);

    let mut lines: Vec<Line> = wrapped_summary(&ticket.summary, content_width)
        .into_iter()
        .map(Line::from)
        .collect();
    if show_assignee {
        lines.push(Line::from(assignee_line(ticket)));
    }

    let paragraph = Paragraph::new(lines).style(style).block(block);
    frame.render_widget(paragraph, area);
}

/// The assignee line shown on a card when the active filter isn't `Me`:
/// `Assignee: <display name>`, or `Assignee: Unassigned` when the ticket has
/// no assignee.
fn assignee_line(ticket: &TicketSummary) -> String {
    format!(
        "Assignee: {}",
        ticket.assignee.as_deref().unwrap_or("Unassigned")
    )
}

/// The rendered height of `ticket`'s card at `content_width`: its wrapped,
/// capped summary plus top/bottom border rows, plus one more row for the
/// assignee line when `show_assignee` is set.
fn card_height(ticket: &TicketSummary, content_width: usize, show_assignee: bool) -> u16 {
    let lines = wrapped_summary(&ticket.summary, content_width).len() as u16;
    let assignee_row = if show_assignee { 1 } else { 0 };
    lines + assignee_row + CARD_BORDER_ROWS
}

/// Word-wrap `summary` to `width` columns, hard-breaking any single word
/// longer than `width`, then cap the result at [`MAX_SUMMARY_LINES`] lines,
/// truncating the last line with a trailing `…` if anything was cut.
fn wrapped_summary(summary: &str, width: usize) -> Vec<String> {
    cap_lines(wrap_text(summary, width), width)
}

/// Greedy word-wrap of `text` to `width` columns (character count, not
/// display width). Words longer than `width` are hard-broken across lines.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();

    for word in text.split_whitespace() {
        let word_chars: Vec<char> = word.chars().collect();
        let mut remaining: &[char] = &word_chars;
        loop {
            if remaining.is_empty() {
                break;
            }
            if current.is_empty() && remaining.len() > width {
                let (head, tail) = remaining.split_at(width);
                lines.push(head.iter().collect());
                remaining = tail;
                continue;
            }
            let needed = if current.is_empty() {
                remaining.len()
            } else {
                current.chars().count() + 1 + remaining.len()
            };
            if needed <= width {
                if !current.is_empty() {
                    current.push(' ');
                }
                current.push_str(&remaining.iter().collect::<String>());
                remaining = &[];
            } else {
                lines.push(std::mem::take(&mut current));
            }
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Truncate `lines` to [`MAX_SUMMARY_LINES`], ellipsizing the last line if
/// anything was cut.
fn cap_lines(lines: Vec<String>, width: usize) -> Vec<String> {
    if lines.len() <= MAX_SUMMARY_LINES {
        return lines;
    }
    let width = width.max(1);
    let mut capped: Vec<String> = lines.into_iter().take(MAX_SUMMARY_LINES).collect();
    let last = capped.last_mut().expect("MAX_SUMMARY_LINES is non-zero");
    let mut chars: Vec<char> = last.chars().collect();
    let keep = width.saturating_sub(1);
    if chars.len() > keep {
        chars.truncate(keep);
    }
    let mut truncated: String = chars.into_iter().collect();
    truncated.push('…');
    *last = truncated;
    capped
}

/// The range of ticket indices to render so that `selected` is fully
/// visible within `available` rows.
///
/// Starts from the top of the column and, if the selected card wouldn't
/// fully fit, advances the start forward just enough that it does (a
/// standard "scroll to keep the cursor visible" viewport). Once the start is
/// fixed, the range is extended forward to fill any remaining space with
/// subsequent cards.
fn visible_card_range(heights: &[u16], selected: usize, available: u16) -> std::ops::Range<usize> {
    if heights.is_empty() {
        return 0..0;
    }
    let selected = selected.min(heights.len() - 1);

    let mut start = 0usize;
    while start < selected {
        let sum: u16 = heights[start..=selected].iter().sum();
        if sum <= available {
            break;
        }
        start += 1;
    }

    let mut end = start;
    let mut used = 0u16;
    for height in &heights[start..] {
        if end > start && used + height > available {
            break;
        }
        used += height;
        end += 1;
        if used >= available {
            break;
        }
    }
    // Always show at least the selected card, even if it alone overflows
    // `available` (a terminal too short for a full card).
    end = end.max(selected + 1);

    start..end
}

/// The priority (stack-rank) screen: every open ticket in the project as a
/// single vertical list, in Jira backlog rank order. Each row shows its rank
/// position, key, status, assignee, and summary. The grabbed row (if any)
/// carries a leading marker distinguishing it from a merely-highlighted row.
fn draw_rank_list(frame: &mut Frame, app: &App, area: Rect) {
    let title = if app.is_rank_grabbed() {
        "Priority (grabbed - j/k move, Enter/Space drop, Esc cancel)"
    } else {
        "Priority"
    };

    let items: Vec<ListItem> = app
        .rank_tickets
        .iter()
        .enumerate()
        .map(|(index, ticket)| {
            let marker = if app.rank_grab_origin.is_some() && app.rank_selected == index {
                "><"
            } else {
                "  "
            };
            let assignee = ticket.assignee.as_deref().unwrap_or("Unassigned");
            ListItem::new(format!(
                "{marker} {:>3}. {}  [{}]  {}  {}",
                index + 1,
                ticket.key,
                ticket.status,
                assignee,
                ticket.summary
            ))
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    let mut state = ListState::default();
    if !app.rank_tickets.is_empty() {
        state.select(Some(app.rank_selected.min(app.rank_tickets.len() - 1)));
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
        Line::from("f           filter by assignee (board only)"),
        Line::from("p           priority (stack-rank) view (board only)"),
        Line::from("Enter/Space grab or drop a ticket (priority view only)"),
        Line::from("?           toggle this help"),
        Line::from(""),
        Line::from("press any key to close"),
    ];
    let paragraph =
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("Help"));
    frame.render_widget(Clear, area);
    frame.render_widget(paragraph, area);
}

/// A centered floating window listing the board's assignee filter options
/// (`Me`, `Unassigned`, `Everyone`, then each assignable user). The currently
/// active filter is marked with a leading `*`. While assignable users are
/// still loading, a loading line is shown in place of the user options; if
/// the last fetch failed, the error is shown instead.
fn draw_filter_picker(frame: &mut Frame, app: &App) {
    let area = centered_rect(50, 50, frame.area());
    frame.render_widget(Clear, area);

    let options = app.filter_options();
    let mut items: Vec<ListItem> = options
        .iter()
        .map(|option| {
            let marker = if option == &app.filter { "* " } else { "  " };
            ListItem::new(format!("{marker}{}", option.label()))
        })
        .collect();

    if app.assignable_users.is_none() {
        match &app.filter_picker_error {
            Some(err) => items.push(ListItem::new(format!("Error: {err}"))),
            None => items.push(ListItem::new("Loading users...")),
        }
    }

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Filter: assignee"),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    let mut state = ListState::default();
    if !options.is_empty() {
        state.select(Some(app.filter_picker_selected.min(options.len() - 1)));
    }
    frame.render_stateful_widget(list, area, &mut state);
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
    use crate::tui::app::{Column, TicketSummary, group_into_columns};
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
            assignee: None,
        }
    }

    fn ticket_with(key: &str, status: &str, status_category: &str) -> TicketSummary {
        TicketSummary {
            status: status.to_string(),
            status_category: status_category.to_string(),
            ..ticket(key)
        }
    }

    fn ticket_with_summary(key: &str, summary: &str) -> TicketSummary {
        TicketSummary {
            summary: summary.to_string(),
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
        render_with_size(app, 80, 24)
    }

    fn render_with_size(app: &App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let backend = TestBackend::new(width, height);
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

    #[test]
    fn long_ticket_summary_wraps_across_multiple_lines_and_is_capped_with_ellipsis() {
        let app = App {
            columns: group_into_columns(vec![ticket_with_summary(
                "PROJ-1",
                "Alpha Bravo Charlie Delta Echo Foxtrot Golf Hotel India Juliett Kilo Lima Mike November Oscar",
            )]),
            ..App::new()
        };
        // Narrow terminal forces a narrow column, which forces wrapping well
        // before the cap is reached.
        let text = buffer_text(&render_with_size(&app, 30, 24));
        assert!(text.contains("Alpha Bravo"));
        // The cap (3 wrapped lines) is well short of every word, so the
        // last visible line must be truncated with a trailing ellipsis.
        assert!(text.contains("…"));
        assert!(!text.contains("Oscar"));
    }

    #[test]
    fn two_tickets_in_a_column_render_as_visually_separated_cards() {
        let app = App {
            columns: group_into_columns(vec![ticket("PROJ-1"), ticket("PROJ-2")]),
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        // Each card is a rounded-corner block; two tickets means two
        // top-left corners.
        assert_eq!(text.matches('╭').count(), 2);
        assert!(text.contains("PROJ-1"));
        assert!(text.contains("PROJ-2"));
    }

    #[test]
    fn selected_card_is_styled_distinctly_from_unselected_card() {
        let app = App {
            columns: group_into_columns(vec![ticket("PROJ-1"), ticket("PROJ-2")]),
            selected_col: 0,
            selected_row: 1,
            ..App::new()
        };
        let buffer = render(&app);

        let modifier_at = |needle: &str| -> Modifier {
            for y in 0..buffer.area.height {
                for x in 0..buffer.area.width {
                    let cell = &buffer[(x, y)];
                    if needle.starts_with(cell.symbol()) && !cell.symbol().trim().is_empty() {
                        // Confirm this is really the start of `needle` by
                        // reading forward.
                        let mut found = String::new();
                        for dx in 0..needle.chars().count() as u16 {
                            if x + dx >= buffer.area.width {
                                break;
                            }
                            found.push_str(buffer[(x + dx, y)].symbol());
                        }
                        if found == needle {
                            return cell.modifier;
                        }
                    }
                }
            }
            Modifier::empty()
        };

        let selected_modifier = modifier_at("PROJ-2");
        let unselected_modifier = modifier_at("PROJ-1");
        assert!(selected_modifier.contains(Modifier::REVERSED));
        assert!(!unselected_modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn scrolling_keeps_selected_card_visible_near_the_bottom_of_a_long_column() {
        let tickets: Vec<TicketSummary> = (1..=20)
            .map(|n| ticket_with_summary(&format!("T{n}"), "short"))
            .collect();
        let app = App {
            columns: group_into_columns(tickets),
            selected_col: 0,
            selected_row: 19,
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        // The last ticket must be visible...
        assert!(text.contains("T20"));
        // ...which, given how many cards fit, means the column must have
        // scrolled past the first ticket.
        assert!(!text.contains("T1 "));
    }

    #[test]
    fn empty_column_among_populated_columns_renders_without_panicking() {
        let app = App {
            columns: vec![
                Column {
                    title: "To Do".to_string(),
                    tickets: vec![],
                },
                Column {
                    title: "Done".to_string(),
                    tickets: vec![ticket("PROJ-1")],
                },
            ],
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        assert!(text.contains("To Do (0)"));
        assert!(text.contains("Done (1)"));
    }

    #[test]
    fn terminal_too_short_for_a_full_card_does_not_panic() {
        let app = App {
            columns: group_into_columns(vec![ticket("PROJ-1")]),
            ..App::new()
        };
        let _ = render_with_size(&app, 80, 3);
    }

    #[test]
    fn very_narrow_column_does_not_panic() {
        let app = App {
            columns: group_into_columns(vec![ticket("PROJ-1"), ticket("PROJ-2")]),
            ..App::new()
        };
        let _ = render_with_size(&app, 4, 24);
    }

    fn jira_user(account_id: &str, display_name: &str) -> crate::jira::types::JiraUser {
        crate::jira::types::JiraUser {
            account_id: account_id.to_string(),
            display_name: display_name.to_string(),
        }
    }

    #[test]
    fn draws_filter_picker_overlay_with_options_and_active_marker() {
        let app = App {
            show_filter_picker: true,
            filter: AssigneeFilter::Unassigned,
            assignable_users: Some(vec![jira_user("acct-1", "Jane Doe")]),
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        assert!(text.contains("Filter: assignee"));
        assert!(text.contains("Me"));
        assert!(text.contains("* Unassigned"));
        assert!(text.contains("Everyone"));
        assert!(text.contains("Jane Doe"));
    }

    #[test]
    fn draws_filter_picker_loading_line_when_users_uncached() {
        let app = App {
            show_filter_picker: true,
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        assert!(text.contains("Loading users"));
    }

    #[test]
    fn draws_filter_picker_error_line_when_last_fetch_failed() {
        let app = App {
            show_filter_picker: true,
            filter_picker_error: Some("boom".to_string()),
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        assert!(text.contains("Error: boom"));
    }

    #[test]
    fn status_bar_shows_active_filter_when_not_me() {
        let app = App {
            filter: AssigneeFilter::Everyone,
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        assert!(text.contains("Filter: Everyone"));
    }

    #[test]
    fn status_bar_omits_filter_when_me() {
        let app = App::new();
        let text = buffer_text(&render(&app));
        assert!(!text.contains("Filter:"));
    }

    #[test]
    fn card_shows_assignee_line_when_filter_is_not_me() {
        let assigned = TicketSummary {
            assignee: Some("Jane Doe".to_string()),
            ..ticket("PROJ-1")
        };
        let unassigned = TicketSummary {
            key: "PROJ-2".to_string(),
            ..ticket("PROJ-2")
        };
        let app = App {
            columns: group_into_columns(vec![assigned, unassigned]),
            filter: AssigneeFilter::Everyone,
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        assert!(text.contains("Assignee: Jane Doe"));
        assert!(text.contains("Assignee: Unassigned"));
    }

    #[test]
    fn card_omits_assignee_line_when_filter_is_me() {
        let app = App {
            columns: group_into_columns(vec![ticket("PROJ-1")]),
            filter: AssigneeFilter::Me,
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        assert!(!text.contains("Assignee:"));
    }

    fn ranked_ticket(key: &str, status: &str, assignee: Option<&str>) -> TicketSummary {
        TicketSummary {
            status: status.to_string(),
            assignee: assignee.map(str::to_string),
            ..ticket(key)
        }
    }

    #[test]
    fn draws_rank_screen_with_position_key_status_assignee_and_summary() {
        let app = App {
            screen: Screen::Rank,
            rank_tickets: vec![
                ranked_ticket("PROJ-1", "To Do", Some("Jane Doe")),
                ranked_ticket("PROJ-2", "In Progress", None),
            ],
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        assert!(text.contains("Priority"));
        assert!(text.contains("1."));
        assert!(text.contains("PROJ-1"));
        assert!(text.contains("To Do"));
        assert!(text.contains("Jane Doe"));
        assert!(text.contains("2."));
        assert!(text.contains("PROJ-2"));
        assert!(text.contains("In Progress"));
        assert!(text.contains("Unassigned"));
        assert!(text.contains("Fix the thing"));
    }

    #[test]
    fn rank_screen_does_not_show_board_columns_behind_it() {
        let app = App {
            screen: Screen::Rank,
            rank_tickets: vec![ranked_ticket("PROJ-1", "To Do", None)],
            ..three_column_app()
        };
        let text = buffer_text(&render(&app));
        // The board columns from `three_column_app` must not leak through;
        // the rank screen is a full replacement, not an overlay.
        assert!(!text.contains("To Do (1)"));
        assert!(!text.contains("In Progress (1)"));
        assert!(!text.contains("Done (1)"));
    }

    #[test]
    fn draws_rank_screen_empty_list_without_panicking() {
        let app = App {
            screen: Screen::Rank,
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        assert!(text.contains("Priority"));
    }

    #[test]
    fn grabbed_row_shows_a_distinct_marker_and_title() {
        let app = App {
            screen: Screen::Rank,
            rank_tickets: vec![
                ranked_ticket("PROJ-1", "To Do", None),
                ranked_ticket("PROJ-2", "To Do", None),
            ],
            rank_selected: 0,
            rank_grab_origin: Some(0),
            rank_snapshot: Some(vec![
                ranked_ticket("PROJ-1", "To Do", None),
                ranked_ticket("PROJ-2", "To Do", None),
            ]),
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        assert!(text.contains("><"));
        assert!(text.contains("grabbed"));
    }

    #[test]
    fn rank_screen_status_bar_never_shows_the_board_filter_label() {
        let app = App {
            screen: Screen::Rank,
            filter: AssigneeFilter::Everyone,
            rank_tickets: vec![ranked_ticket("PROJ-1", "To Do", None)],
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        assert!(!text.contains("Filter:"));
    }

    #[test]
    fn help_overlay_documents_priority_view_keys() {
        let app = App {
            show_help: true,
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        assert!(text.contains("priority"));
        assert!(text.contains("grab"));
    }
}
