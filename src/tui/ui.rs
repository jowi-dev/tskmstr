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
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap,
};

use crate::cli::runs::format_age;
use crate::runs::RunStatus;
use crate::tui::app::{App, AssigneeFilter, Column, RUN_COLUMNS, RunCard, Screen, TicketSummary};
use crate::tui::theme;

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

    match app.screen {
        Screen::Rank => draw_rank_list(frame, app, body),
        Screen::Runs => draw_runs_board(frame, app, body),
        _ => draw_board_columns(frame, app, body),
    }

    match app.screen {
        Screen::Board | Screen::Rank => {}
        Screen::Detail => draw_detail_window(frame, app),
        Screen::TransitionMenu => {
            draw_detail_window(frame, app);
            draw_transition_window(frame, app);
        }
        Screen::Runs => {
            if app.show_run_detail {
                draw_run_detail_window(frame, app);
            }
        }
    }

    draw_status_bar(
        frame,
        status_area,
        &status_line_text(app),
        hint_for(app.screen, app.show_run_detail),
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
    let hint_span = Span::styled(hints.to_string(), theme::DIM);
    let line = if status_line.is_empty() {
        Line::from(hint_span)
    } else {
        Line::from(vec![Span::raw(format!("{status_line}  |  ")), hint_span])
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// The key-hint text shown in the status bar for `screen`. `show_run_detail`
/// only affects [`Screen::Runs`]'s hint, picking the detail-window variant.
fn hint_for(screen: Screen, show_run_detail: bool) -> &'static str {
    match screen {
        Screen::Board => {
            "h/l column  j/k move  Enter open  r refresh  o browser  f filter  p priority  ? help  q quit"
        }
        Screen::Detail => "j/k scroll  Enter transitions  Esc back  ? help  q quit",
        Screen::TransitionMenu => "j/k move  Enter apply  Esc back  ? help  q quit",
        Screen::Rank => {
            "j/k move  Enter/Space grab-drop  r refresh  o browser  Esc back  ? help  q quit"
        }
        Screen::Runs if show_run_detail => "j/k scroll  Esc/q close  r refresh  q quit",
        Screen::Runs => "h/l/j/k: move  enter: detail  r: refresh  q: quit",
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
    let title = Line::from(Span::styled(
        format!("{} ({})", column.title, column.tickets.len()),
        theme::COLUMN_TITLE,
    ));
    let border_style = if is_selected {
        theme::SELECTED_COLUMN_BORDER
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
    let key_style = style.patch(theme::CARD_KEY);

    let block = Block::default()
        .border_type(BorderType::Rounded)
        .borders(Borders::ALL)
        .title(Line::from(Span::styled(ticket.key.clone(), key_style)))
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
            let grabbed = app.rank_grab_origin.is_some() && app.rank_selected == index;
            let marker_span = if grabbed {
                Span::styled("><", theme::GRABBED_MARKER)
            } else {
                Span::raw("  ")
            };
            let assignee = ticket.assignee.as_deref().unwrap_or("Unassigned");
            let key_span = Span::styled(
                format!(" {:>3}. {}", index + 1, ticket.key),
                theme::CARD_KEY,
            );
            let rest_span = Span::raw(format!(
                "  [{}]  {}  {}",
                ticket.status, assignee, ticket.summary
            ));
            ListItem::new(Line::from(vec![marker_span, key_span, rest_span]))
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

/// How many seconds without a heartbeat before a running card is marked
/// stale with a leading `!`. Matches `tm runs reap`'s reasoning, though not
/// its default threshold: this is purely a visual warning, not a reap
/// decision.
const STALE_HEARTBEAT_SECS: i64 = 600;

/// The runs kanban board: one bordered column per [`RUN_COLUMNS`] entry,
/// always all six, even when empty.
fn draw_runs_board(frame: &mut Frame, app: &App, area: Rect) {
    let column_count = RUN_COLUMNS.len() as u32;
    let constraints: Vec<Constraint> = (0..column_count)
        .map(|_| Constraint::Ratio(1, column_count))
        .collect();
    let areas = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(area);

    for (index, status) in RUN_COLUMNS.iter().enumerate() {
        let cards = app.runs_in_col(index);
        let is_selected_column = index == app.runs_selected_col;
        let selected_row = is_selected_column.then_some(app.runs_selected_row);
        draw_run_column(frame, areas[index], *status, &cards, selected_row);
    }
}

/// One column of the runs board: a bordered, titled stack of run cards.
fn draw_run_column(
    frame: &mut Frame,
    area: Rect,
    status: RunStatus,
    cards: &[&RunCard],
    selected_row: Option<usize>,
) {
    let title_style = theme::run_status_style(status).add_modifier(Modifier::BOLD);
    let title = Line::from(Span::styled(
        format!("{status:?} ({})", cards.len()),
        title_style,
    ));
    let border_style = if selected_row.is_some() {
        theme::SELECTED_COLUMN_BORDER
    } else {
        Style::default()
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(border_style);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if cards.is_empty() || inner.width == 0 || inner.height == 0 {
        return;
    }

    let constraints: Vec<Constraint> = cards.iter().map(|_| Constraint::Length(3)).collect();
    let card_areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    for (index, card) in cards.iter().enumerate() {
        let is_selected = selected_row == Some(index);
        draw_run_card(frame, card_areas[index], card, is_selected);
    }
}

/// One run's card: three lines (ticket, lane, age/last-event), reversed when
/// selected. A running card whose heartbeat is older than
/// [`STALE_HEARTBEAT_SECS`] gets a trailing red `!` on its ticket line.
fn draw_run_card(frame: &mut Frame, area: Rect, card: &RunCard, is_selected: bool) {
    let style = if is_selected {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
    };

    let stale = card.status == RunStatus::Running
        && card
            .heartbeat_age_secs
            .is_some_and(|age| age > STALE_HEARTBEAT_SECS);

    let mut ticket_spans = vec![Span::styled(
        card.ticket.clone(),
        style.patch(theme::CARD_KEY),
    )];
    if stale {
        ticket_spans.push(Span::styled(" !", style.patch(theme::STALE_MARKER)));
    }

    let last_event = match (&card.last_event_kind, card.last_event_age_secs) {
        (Some(kind), Some(age)) => format!("{kind} {}", format_age(age)),
        _ => "-".to_string(),
    };

    let mut lane_line = card.lane.clone();
    if let Some(checklist) = &card.checklist {
        lane_line = format!(
            "{lane_line}  {}/{}",
            checklist.done_count(),
            checklist.items.len()
        );
    }
    let mut lane_spans = vec![Span::styled(lane_line, style)];
    if let Some(badge) = theme::kind_badge(&card.kind) {
        lane_spans.push(Span::styled(badge.content, style.patch(badge.style)));
    }

    let lines = vec![
        Line::from(ticket_spans),
        Line::from(lane_spans),
        Line::from(Span::styled(
            format!("{} · {last_event}", format_age(card.age_secs)),
            style.patch(theme::DIM),
        )),
    ];

    frame.render_widget(Paragraph::new(lines).style(style), area);
}

/// A centered floating window (~80% wide, ~70% tall) showing the selected
/// run's full detail and event timeline, drawn over the runs board.
///
/// While `app.run_detail` is still loading (`None`), shows a placeholder
/// instead. Event lines are truncated to width rather than wrapped, and
/// scrolled by `app.run_detail_scroll`.
fn draw_run_detail_window(frame: &mut Frame, app: &App) {
    let area = centered_rect(80, 70, frame.area());
    frame.render_widget(Clear, area);

    let Some(detail) = &app.run_detail else {
        let paragraph = Paragraph::new("Loading...").block(
            Block::default()
                .borders(Borders::ALL)
                .title(bold_title("Run detail")),
        );
        frame.render_widget(paragraph, area);
        return;
    };

    let title = if detail.kind == "lane" {
        format!("Run {}: {}", detail.id, detail.ticket)
    } else {
        format!("Run {}: {} ({})", detail.id, detail.ticket, detail.kind)
    };

    let mut lines = vec![
        Line::from(format!("lane: {}", detail.lane)),
        Line::from(format!("status: {:?}", detail.status)),
        Line::from(format!("worktree: {}", detail.worktree)),
    ];
    if let Some(branch) = &detail.branch {
        lines.push(Line::from(format!("branch: {branch}")));
    }
    if let Some(pid) = detail.pid {
        lines.push(Line::from(format!("pid: {pid}")));
    }
    if let Some(session_id) = &detail.session_id {
        lines.push(Line::from(format!("session: {session_id}")));
    }
    if let Some(cost) = detail.cost_usd {
        lines.push(Line::from(format!("cost: ${cost:.2}")));
    }
    if let Some(turns) = detail.num_turns {
        lines.push(Line::from(format!("turns: {turns}")));
    }
    if let Some(pr_url) = &detail.pr_url {
        lines.push(Line::from(format!("pr: {pr_url}")));
    }
    if let Some(blocker) = &detail.blocker {
        lines.push(Line::from(format!("blocker: {blocker}")));
    }
    lines.push(Line::from(format!("started: {}", detail.started_at)));
    if let Some(ended_at) = &detail.ended_at {
        lines.push(Line::from(format!("ended: {ended_at}")));
    }
    if let Some(tools_line) = crate::runs::format_tool_counts(&detail.tool_counts) {
        lines.push(Line::from(Span::styled(tools_line, theme::SECTION_HEADER)));
    }

    if let Some(usage) = &detail.model_usage {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(usage.label, theme::SECTION_HEADER)));
        for line in &usage.lines {
            lines.push(Line::from(line.clone()));
        }
    }
    lines.push(Line::from(""));

    if let Some(checklist) = &detail.checklist {
        lines.push(Line::from(Span::styled(
            format!(
                "Checklist ({}/{} done)",
                checklist.done_count(),
                checklist.items.len()
            ),
            theme::SECTION_HEADER,
        )));
        for item in &checklist.items {
            let (marker, item_style) = if item.done {
                ("[x]", theme::CHECKLIST_DONE)
            } else {
                ("[ ]", theme::CHECKLIST_PENDING)
            };
            lines.push(Line::from(Span::styled(
                format!("{marker} {}", item.text),
                item_style,
            )));
        }
        lines.push(Line::from(""));
    }

    if detail.events.is_empty() {
        lines.push(Line::from("(no events)"));
    } else {
        for event in detail.events.iter().rev() {
            let rest = match crate::runs::format_event_detail(&event.kind, event.detail.as_deref())
            {
                Some(friendly) => format!("{}  {friendly}", event.kind),
                None => match &event.detail {
                    Some(d) => format!("{}  {d}", event.kind),
                    None => event.kind.clone(),
                },
            };
            lines.push(Line::from(vec![
                Span::styled(event.at.clone(), theme::DIM),
                Span::raw(format!("  {rest}")),
            ]));
        }
    }

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(bold_title(title)),
        )
        .scroll((app.run_detail_scroll, 0));
    frame.render_widget(paragraph, area);
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
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(bold_title(title)),
        )
        .wrap(Wrap { trim: false })
        .scroll((app.detail_scroll, 0));
    frame.render_widget(paragraph, area);
}

/// A window title rendered bold, for every floating window's border title.
fn bold_title(title: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(title.into(), theme::BOLD))
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
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(bold_title("Move to")),
        )
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
        Line::from("r           refresh tickets (inert mid-grab in priority view)"),
        Line::from("o           open in browser"),
        Line::from("f           filter by assignee (board only)"),
        Line::from("p           priority (stack-rank) view (board only)"),
        Line::from("Enter/Space grab or drop a ticket (priority view only)"),
        Line::from("?           toggle this help"),
        Line::from(""),
        Line::from("press any key to close"),
    ];
    let paragraph = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(bold_title("Help")),
    );
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
            let line = if option == &app.filter {
                Line::from(vec![
                    Span::styled("* ", theme::ACTIVE_FILTER_MARKER),
                    Span::raw(option.label()),
                ])
            } else {
                Line::from(format!("  {}", option.label()))
            };
            ListItem::new(line)
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
                .title(bold_title("Filter: assignee")),
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
            columns: group_into_columns(
                vec![
                    ticket_with("PROJ-1", "To Do", "new"),
                    ticket_with("PROJ-2", "In Progress", "indeterminate"),
                    ticket_with("PROJ-3", "Done", "done"),
                ],
                &[],
            ),
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
            columns: group_into_columns(vec![ticket("PROJ-1")], &[]),
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
    fn board_hint_lists_every_board_key() {
        let hint = hint_for(Screen::Board, false);
        for key in [
            "h/l",
            "j/k",
            "Enter",
            "r",
            "o",
            "f filter",
            "p priority",
            "?",
            "q",
        ] {
            assert!(
                hint.contains(key),
                "board hint should mention `{key}`: {hint}"
            );
        }
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
            columns: group_into_columns(vec![ticket("PROJ-1")], &[]),
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
            columns: group_into_columns(vec![ticket("PROJ-1")], &[]),
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
            columns: group_into_columns(vec![ticket("PROJ-1")], &[]),
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
            columns: group_into_columns(
                vec![ticket_with_summary(
                    "PROJ-1",
                    "Alpha Bravo Charlie Delta Echo Foxtrot Golf Hotel India Juliett Kilo Lima Mike November Oscar",
                )],
                &[],
            ),
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
            columns: group_into_columns(vec![ticket("PROJ-1"), ticket("PROJ-2")], &[]),
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        // Each card is a rounded-corner block; two tickets means two
        // top-left corners.
        assert_eq!(text.matches('╭').count(), 2);
        assert!(text.contains("PROJ-1"));
        assert!(text.contains("PROJ-2"));
    }

    /// Find the first cell at which `needle` starts rendering (reading
    /// left-to-right, top-to-bottom), verifying the full string actually
    /// matches from that point rather than just its first character.
    fn cell_at<'a>(
        buffer: &'a ratatui::buffer::Buffer,
        needle: &str,
    ) -> Option<&'a ratatui::buffer::Cell> {
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                let cell = &buffer[(x, y)];
                if needle.starts_with(cell.symbol()) && !cell.symbol().trim().is_empty() {
                    let mut found = String::new();
                    for dx in 0..needle.chars().count() as u16 {
                        if x + dx >= buffer.area.width {
                            break;
                        }
                        found.push_str(buffer[(x + dx, y)].symbol());
                    }
                    if found == needle {
                        return Some(cell);
                    }
                }
            }
        }
        None
    }

    /// The [`Modifier`] of the cell at which `needle` starts rendering, or
    /// [`Modifier::empty`] if it isn't found.
    fn modifier_at(buffer: &ratatui::buffer::Buffer, needle: &str) -> Modifier {
        cell_at(buffer, needle)
            .map(|cell| cell.modifier)
            .unwrap_or_else(Modifier::empty)
    }

    #[test]
    fn selected_card_is_styled_distinctly_from_unselected_card() {
        let app = App {
            columns: group_into_columns(vec![ticket("PROJ-1"), ticket("PROJ-2")], &[]),
            selected_col: 0,
            selected_row: 1,
            ..App::new()
        };
        let buffer = render(&app);

        let selected_modifier = modifier_at(&buffer, "PROJ-2");
        let unselected_modifier = modifier_at(&buffer, "PROJ-1");
        assert!(selected_modifier.contains(Modifier::REVERSED));
        assert!(!unselected_modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn scrolling_keeps_selected_card_visible_near_the_bottom_of_a_long_column() {
        let tickets: Vec<TicketSummary> = (1..=20)
            .map(|n| ticket_with_summary(&format!("T{n}"), "short"))
            .collect();
        let app = App {
            columns: group_into_columns(tickets, &[]),
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
            columns: group_into_columns(vec![ticket("PROJ-1")], &[]),
            ..App::new()
        };
        let _ = render_with_size(&app, 80, 3);
    }

    #[test]
    fn very_narrow_column_does_not_panic() {
        let app = App {
            columns: group_into_columns(vec![ticket("PROJ-1"), ticket("PROJ-2")], &[]),
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
            columns: group_into_columns(vec![assigned, unassigned], &[]),
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
            columns: group_into_columns(vec![ticket("PROJ-1")], &[]),
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

    fn run_card(id: i64, ticket: &str, lane: &str, status: RunStatus) -> RunCard {
        RunCard {
            id,
            ticket: ticket.to_string(),
            lane: lane.to_string(),
            kind: "lane".to_string(),
            status,
            age_secs: 90,
            heartbeat_age_secs: Some(30),
            last_event_kind: Some("tool_use".to_string()),
            last_event_age_secs: Some(5),
            checklist: None,
        }
    }

    fn runs_app(cards: Vec<RunCard>) -> App {
        App {
            screen: Screen::Runs,
            runs: cards,
            ..App::new()
        }
    }

    #[test]
    fn draws_runs_board_with_all_six_column_titles() {
        let app = runs_app(vec![]);
        let text = buffer_text(&render(&app));
        assert!(text.contains("Queued (0)"));
        assert!(text.contains("Running (0)"));
        assert!(text.contains("Blocked (0)"));
        assert!(text.contains("Review (0)"));
        assert!(text.contains("Done (0)"));
        assert!(text.contains("Failed (0)"));
    }

    #[test]
    fn draws_runs_board_with_a_cards_ticket_and_lane_visible() {
        let app = runs_app(vec![run_card(1, "PROJ-1", "backend", RunStatus::Running)]);
        let text = buffer_text(&render(&app));
        assert!(text.contains("Running (1)"));
        assert!(text.contains("PROJ-1"));
        assert!(text.contains("backend"));
    }

    #[test]
    fn running_column_title_carries_its_status_color() {
        let app = runs_app(vec![run_card(1, "PROJ-1", "backend", RunStatus::Running)]);
        let buffer = render(&app);
        let cell = cell_at(&buffer, "Running (1)").expect("Running column title renders");
        assert_eq!(
            Some(cell.fg),
            theme::run_status_style(RunStatus::Running).fg
        );
    }

    #[test]
    fn non_lane_run_card_renders_its_kind_badge_styled() {
        let card = RunCard {
            kind: "audit".to_string(),
            ..run_card(1, "PROJ-1", "backend", RunStatus::Running)
        };
        // A wide terminal so the kanban columns are wide enough for the lane
        // name plus the appended kind badge to render unclipped.
        let buffer = render_with_size(&runs_app(vec![card]), 160, 24);
        let text = buffer_text(&buffer);
        assert!(text.contains("backend audit"));
        let cell = cell_at(&buffer, "audit").expect("kind badge text renders");
        assert_eq!(Some(cell.fg), theme::kind_style("audit").fg);
    }

    #[test]
    fn lane_run_card_renders_no_kind_badge() {
        let card = run_card(1, "PROJ-1", "backend", RunStatus::Running);
        assert_eq!(card.kind, "lane");
        let text = buffer_text(&render_with_size(&runs_app(vec![card]), 160, 24));
        let lane_line = text
            .lines()
            .find(|line| line.contains("backend"))
            .expect("expected a line with the card's lane");
        assert!(!lane_line.contains("audit"));
        assert!(!lane_line.contains("create"));
    }

    #[test]
    fn stale_running_card_shows_a_marker() {
        let stale = RunCard {
            heartbeat_age_secs: Some(601),
            ..run_card(1, "PROJ-1", "backend", RunStatus::Running)
        };
        let text = buffer_text(&render(&runs_app(vec![stale])));
        assert!(text.contains('!'));
    }

    #[test]
    fn fresh_running_card_shows_no_stale_marker() {
        let fresh = RunCard {
            heartbeat_age_secs: Some(30),
            ..run_card(1, "PROJ-1", "backend", RunStatus::Running)
        };
        let text = buffer_text(&render(&runs_app(vec![fresh])));
        assert!(!text.contains('!'));
    }

    #[test]
    fn run_detail_overlay_leaves_column_titles_visible() {
        let app = App {
            show_run_detail: true,
            run_detail: None,
            ..runs_app(vec![run_card(1, "PROJ-1", "backend", RunStatus::Running)])
        };
        let text = buffer_text(&render(&app));
        assert!(text.contains("Running (1)"));
        assert!(text.contains("Loading..."));
    }

    #[test]
    fn run_detail_overlay_shows_loaded_fields_and_events() {
        let detail = crate::tui::app::RunDetail {
            id: 1,
            ticket: "PROJ-1".to_string(),
            lane: "backend".to_string(),
            kind: "lane".to_string(),
            status: RunStatus::Running,
            worktree: "/tmp/wt".to_string(),
            branch: Some("proj-1".to_string()),
            pid: Some(4242),
            session_id: Some("sess-abc".to_string()),
            cost_usd: Some(1.5),
            num_turns: Some(3),
            pr_url: None,
            blocker: None,
            started_at: "2020-01-01T00:00:00.000Z".to_string(),
            ended_at: None,
            events: vec![crate::tui::app::RunDetailEvent {
                at: "2020-01-01T00:00:01.000Z".to_string(),
                kind: "tool_use".to_string(),
                detail: None,
            }],
            checklist: None,
            tool_counts: vec![],
            model_usage: None,
        };
        let app = App {
            show_run_detail: true,
            run_detail: Some(detail),
            ..runs_app(vec![run_card(1, "PROJ-1", "backend", RunStatus::Running)])
        };
        let text = buffer_text(&render(&app));
        assert!(text.contains("Run 1: PROJ-1"));
        assert!(text.contains("sess-abc"));
        assert!(text.contains("tool_use"));
    }

    #[test]
    fn run_detail_overlay_renders_events_newest_first() {
        let detail = crate::tui::app::RunDetail {
            events: vec![
                crate::tui::app::RunDetailEvent {
                    at: "2020-01-01T00:00:01.000Z".to_string(),
                    kind: "first".to_string(),
                    detail: None,
                },
                crate::tui::app::RunDetailEvent {
                    at: "2020-01-01T00:00:02.000Z".to_string(),
                    kind: "second".to_string(),
                    detail: None,
                },
            ],
            ..run_detail_fixture()
        };
        let app = App {
            show_run_detail: true,
            run_detail: Some(detail),
            ..runs_app(vec![run_card(1, "PROJ-1", "backend", RunStatus::Running)])
        };
        let text = buffer_text(&render(&app));
        let first_pos = text.find("second").expect("second event present");
        let second_pos = text.find("first").expect("first event present");
        assert!(
            first_pos < second_pos,
            "expected newest event (second) to render before oldest event (first): {text}"
        );
    }

    fn checklist(items: &[(&str, bool)]) -> crate::runs::ChecklistState {
        crate::runs::ChecklistState {
            items: items
                .iter()
                .map(|(text, done)| crate::runs::ChecklistItem {
                    text: text.to_string(),
                    done: *done,
                })
                .collect(),
        }
    }

    #[test]
    fn run_detail_overlay_renders_checklist_section_above_events() {
        let detail = crate::tui::app::RunDetail {
            checklist: Some(checklist(&[
                ("write tests", true),
                ("implement", true),
                ("review", false),
            ])),
            ..run_detail_fixture()
        };
        let app = App {
            show_run_detail: true,
            run_detail: Some(detail),
            ..runs_app(vec![run_card(1, "PROJ-1", "backend", RunStatus::Running)])
        };
        let text = buffer_text(&render(&app));
        assert!(text.contains("Checklist (2/3 done)"));
        assert!(text.contains("[x] write tests"));
        assert!(text.contains("[x] implement"));
        assert!(text.contains("[ ] review"));
    }

    #[test]
    fn checklist_done_line_is_green_and_pending_line_is_dim() {
        let detail = crate::tui::app::RunDetail {
            checklist: Some(checklist(&[("write tests", true), ("review", false)])),
            ..run_detail_fixture()
        };
        let app = App {
            show_run_detail: true,
            run_detail: Some(detail),
            ..runs_app(vec![run_card(1, "PROJ-1", "backend", RunStatus::Running)])
        };
        let buffer = render(&app);
        let done_cell = cell_at(&buffer, "[x] write tests").expect("done item renders");
        let pending_cell = cell_at(&buffer, "[ ] review").expect("pending item renders");
        assert_eq!(Some(done_cell.fg), theme::CHECKLIST_DONE.fg);
        assert_eq!(Some(pending_cell.fg), theme::CHECKLIST_PENDING.fg);
    }

    #[test]
    fn run_detail_overlay_with_no_checklist_has_no_checklist_section() {
        let app = App {
            show_run_detail: true,
            run_detail: Some(run_detail_fixture()),
            ..runs_app(vec![run_card(1, "PROJ-1", "backend", RunStatus::Running)])
        };
        let text = buffer_text(&render(&app));
        assert!(!text.contains("Checklist"));
    }

    #[test]
    fn run_detail_overlay_renders_friendly_tool_event_detail() {
        let detail = crate::tui::app::RunDetail {
            events: vec![crate::tui::app::RunDetailEvent {
                at: "2020-01-01T00:00:01.000Z".to_string(),
                kind: "tool".to_string(),
                detail: Some(r#"{"tool":"Bash","summary":"cargo test"}"#.to_string()),
            }],
            ..run_detail_fixture()
        };
        let app = App {
            show_run_detail: true,
            run_detail: Some(detail),
            ..runs_app(vec![run_card(1, "PROJ-1", "backend", RunStatus::Running)])
        };
        let text = buffer_text(&render(&app));
        assert!(text.contains("Bash — cargo test"));
        assert!(!text.contains("\"tool\":\"Bash\""));
    }

    #[test]
    fn run_detail_overlay_shows_tools_summary_line() {
        let detail = crate::tui::app::RunDetail {
            tool_counts: vec![("Bash".to_string(), 2), ("Edit".to_string(), 1)],
            ..run_detail_fixture()
        };
        let app = App {
            show_run_detail: true,
            run_detail: Some(detail),
            ..runs_app(vec![run_card(1, "PROJ-1", "backend", RunStatus::Running)])
        };
        let text = buffer_text(&render(&app));
        assert!(text.contains("Tools: Bash \u{d7}2, Edit \u{d7}1"));
    }

    #[test]
    fn run_detail_overlay_renders_model_usage_section() {
        let detail = crate::tui::app::RunDetail {
            model_usage: Some(crate::tui::app::RunModelUsage {
                label: "Model usage",
                lines: vec!["claude-fable-5  $13.00  out 58.6k, in 146".to_string()],
            }),
            ..run_detail_fixture()
        };
        let app = App {
            show_run_detail: true,
            run_detail: Some(detail),
            ..runs_app(vec![run_card(1, "PROJ-1", "backend", RunStatus::Running)])
        };
        let text = buffer_text(&render(&app));
        assert!(text.contains("Model usage"));
        assert!(text.contains("$13.00"));
    }

    #[test]
    fn run_detail_overlay_with_no_model_usage_has_no_model_usage_section() {
        let app = App {
            show_run_detail: true,
            run_detail: Some(run_detail_fixture()),
            ..runs_app(vec![run_card(1, "PROJ-1", "backend", RunStatus::Running)])
        };
        let text = buffer_text(&render(&app));
        assert!(!text.contains("Model usage"));
    }

    #[test]
    fn run_detail_overlay_with_no_tool_events_has_no_tools_line() {
        let app = App {
            show_run_detail: true,
            run_detail: Some(run_detail_fixture()),
            ..runs_app(vec![run_card(1, "PROJ-1", "backend", RunStatus::Running)])
        };
        let text = buffer_text(&render(&app));
        assert!(!text.contains("Tools:"));
    }

    fn run_detail_fixture() -> crate::tui::app::RunDetail {
        crate::tui::app::RunDetail {
            id: 1,
            ticket: "PROJ-1".to_string(),
            lane: "backend".to_string(),
            kind: "lane".to_string(),
            status: RunStatus::Running,
            worktree: "/tmp/wt".to_string(),
            branch: None,
            pid: None,
            session_id: None,
            cost_usd: None,
            num_turns: None,
            pr_url: None,
            blocker: None,
            started_at: "2020-01-01T00:00:00.000Z".to_string(),
            ended_at: None,
            events: vec![],
            checklist: None,
            tool_counts: vec![],
            model_usage: None,
        }
    }

    #[test]
    fn runs_board_card_shows_checklist_progress() {
        let card = RunCard {
            checklist: Some(checklist(&[
                ("a", true),
                ("b", true),
                ("c", true),
                ("d", false),
            ])),
            ..run_card(1, "PROJ-1", "backend", RunStatus::Running)
        };
        let text = buffer_text(&render(&runs_app(vec![card])));
        assert!(text.contains("3/4"));
    }

    #[test]
    fn runs_board_card_with_no_checklist_shows_no_progress_marker() {
        let app = runs_app(vec![run_card(1, "PROJ-1", "backend", RunStatus::Running)]);
        let text = buffer_text(&render(&app));
        let lane_line = text
            .lines()
            .find(|line| line.contains("backend"))
            .expect("expected a line with the card's lane");
        assert!(!lane_line.contains('/'));
    }

    #[test]
    fn runs_hint_mentions_detail_and_refresh() {
        let hint = hint_for(Screen::Runs, false);
        assert!(hint.contains("enter"));
        assert!(hint.contains("refresh"));
    }
}
