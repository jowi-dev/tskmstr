//! Rendering: pure functions from [`App`] state to widgets on a [`Frame`].
//!
//! Nothing here reads events or performs I/O; `crate::tui::event` is the only
//! module that touches a real terminal. That split is what lets rendering be
//! smoke-tested with ratatui's `TestBackend`.
//!
//! The board is always drawn first, one bordered column per status. The
//! detail and transition-menu screens layer centered floating windows on top
//! of it (via [`Clear`]), so the board stays visible behind them.

use std::collections::HashMap;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap,
};

use crate::cli::runs::format_age;
use crate::runs::{RetroSeverity, RunStatus};
use crate::tui::app::{
    App, AssigneeFilter, AuditStatusEntry, BotWatchIndicator, Column, RETRO_SEVERITIES,
    RUN_COLUMNS, RunCard, RunIndicator, Screen, TicketSummary,
};
use crate::tui::theme;

/// Maximum number of wrapped summary lines shown on a single ticket card.
/// Longer summaries are truncated with a trailing ellipsis so that one long
/// ticket cannot push the rest of the column out of view. Combined with the
/// card's top/bottom border, this caps a single card at 5 rows tall.
const MAX_SUMMARY_LINES: usize = 3;

/// Border rows (top + bottom) added to every card in addition to its wrapped
/// summary lines.
const CARD_BORDER_ROWS: u16 = 2;

/// A board ticket's badge-worthy status maps, bundled so [`draw_column`],
/// [`draw_card`], and [`card_height`] can each take one reference instead of
/// four -- keeping their arity down now that a ticket can carry an audit
/// badge, a lane-run badge (see `docs/plans/board-lane-runs.md`'s "Badges"
/// decision), a bot-watch badge and a cleanup badge (see
/// `docs/plans/bugbot-watch.md`'s "Board integration") all at once.
struct BoardBadges<'a> {
    /// Per-ticket audit badge state, from [`App::audit_status`].
    audit_status: &'a HashMap<String, AuditStatusEntry>,
    /// Per-ticket lane-run badge state, from [`App::lane_run_status`].
    lane_run_status: &'a HashMap<String, RunIndicator>,
    /// Per-ticket PR bot-watch badge state, from [`App::bot_watch_status`].
    bot_watch_status: &'a HashMap<String, BotWatchIndicator>,
    /// Ticket keys with a `tm pr watch` launcher child in flight, from
    /// [`App::pending_bot_watch_launches`]. Renders a starting-style `bots:`
    /// badge for any ticket that has no `bot_watch_status` entry yet -- unlike
    /// lane runs, whose pending state is overlaid reducer-side, because
    /// [`BotWatchIndicator`] deliberately has no `Starting` variant (no watcher
    /// run status maps to one).
    pending_bot_watch: &'a std::collections::HashSet<String>,
    /// Per-ticket bugbot-cleanup badge state, from [`App::cleanup_status`].
    cleanup_status: &'a HashMap<String, AuditStatusEntry>,
}

/// The `bots:` badge to render for `ticket_key`, if any: its loaded
/// [`BotWatchIndicator`], or the starting overlay when a launcher child is
/// still in flight with no run row recorded yet. A loaded run row always wins,
/// for the same "the row is fresher truth than the pending flag" reason
/// [`crate::tui::app::lane_run_indicator`] documents.
fn bot_watch_badge<'a>(badges: &BoardBadges<'a>, ticket_key: &str) -> Option<(&'a str, Style)> {
    match badges.bot_watch_status.get(ticket_key) {
        Some(indicator) => Some((
            theme::bot_watch_indicator_label(*indicator),
            theme::bot_watch_indicator_style(*indicator),
        )),
        None if badges.pending_bot_watch.contains(ticket_key) => {
            Some((theme::BOT_WATCH_STARTING_LABEL, theme::DIM))
        }
        None => None,
    }
}

/// Draw the current screen (and the help overlay, if shown) into `frame`.
pub fn draw(frame: &mut Frame, app: &App) {
    let (body, status_area) = split_body_and_status(frame.area());

    match app.screen {
        Screen::Rank => draw_rank_list(frame, app, body),
        Screen::Runs => draw_runs_board(frame, app, body),
        Screen::Retro => draw_retro_list(frame, app, body),
        _ => draw_board_columns(frame, app, body),
    }

    match app.screen {
        Screen::Board => {
            if app.show_run_detail {
                draw_run_detail_window(frame, app);
            }
        }
        Screen::Rank => {}
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
        Screen::Retro => {}
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

    if app.show_lane_picker {
        draw_lane_picker(frame, app);
    }

    if app.show_browser_picker {
        draw_browser_picker(frame, app);
    }

    if app.show_retro_severity_picker {
        draw_retro_severity_picker(frame, app);
    }

    if app.show_retro_note_entry {
        draw_retro_note_entry(frame, app);
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
/// picks the detail-window variant on both [`Screen::Board`] and
/// [`Screen::Runs`] -- the run-detail overlay's keymap (scroll/close/refresh)
/// is shared between the two hosts (see `docs/plans/board-run-detail.md`'s
/// "Key gating" decision).
fn hint_for(screen: Screen, show_run_detail: bool) -> &'static str {
    match screen {
        Screen::Board if show_run_detail => "j/k scroll  Esc/q close  r refresh",
        Screen::Board => {
            "h/l column  j/k move  Enter open  r refresh  o browser  O jira  f filter  p priority  a audit  s session  w work  b bots  v view run  L logs  V vdiff  F fix  R retro  ? help  q quit"
        }
        Screen::Detail => "j/k scroll  Enter transitions  Esc back  ? help  q quit",
        Screen::TransitionMenu => "j/k move  Enter apply  Esc back  ? help  q quit",
        Screen::Rank => {
            "j/k move  Enter/Space grab-drop  r refresh  o browser  O jira  Esc back  ? help  q quit"
        }
        Screen::Runs if show_run_detail => "j/k scroll  Esc/q close  r refresh  q quit",
        Screen::Runs => "h/l/j/k: move  enter: detail  r: refresh  q: quit",
        Screen::Retro => {
            "j/k move  d defect  c clean  r refresh  o browser  Esc back  ? help  q quit"
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
    let badges = BoardBadges {
        audit_status: &app.audit_status,
        lane_run_status: &app.lane_run_status,
        bot_watch_status: &app.bot_watch_status,
        pending_bot_watch: &app.pending_bot_watch_launches,
        cleanup_status: &app.cleanup_status,
    };

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
            &badges,
        );
    }
}

/// A board column's title style: bold, colored by the status category
/// shared by its tickets (they share a status by construction, so the
/// first ticket's category stands in for the whole column). An empty
/// column has no ticket to derive a category from, so it falls back to the
/// plain [`theme::COLUMN_TITLE`].
fn column_title_style(column: &Column) -> Style {
    match column.tickets.first() {
        Some(ticket) => {
            theme::ticket_status_style(&ticket.status_category).add_modifier(Modifier::BOLD)
        }
        None => theme::COLUMN_TITLE,
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
    badges: &BoardBadges,
) {
    let title = Line::from(Span::styled(
        format!("{} ({})", column.title, column.tickets.len()),
        column_title_style(column),
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
        .map(|t| card_height(t, content_width, show_assignee, badges))
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
            badges,
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
    badges: &BoardBadges,
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
        lines.push(Line::from(Span::styled(assignee_line(ticket), theme::DIM)));
    }
    if let Some(entry) = badges.audit_status.get(&ticket.key) {
        // `style.patch(...)` (mirroring `key_style` above) merges the
        // indicator's fg color/bold onto `style` rather than replacing it,
        // so a selected card's `REVERSED` modifier survives on the badge
        // line too -- the same selection contract every other styled span
        // on this card upholds.
        lines.push(Line::from(Span::styled(
            theme::audit_indicator_label(entry.indicator),
            style.patch(theme::audit_indicator_style(entry.indicator)),
        )));
    }
    if let Some(indicator) = badges.lane_run_status.get(&ticket.key) {
        lines.push(Line::from(Span::styled(
            theme::run_indicator_label(*indicator),
            style.patch(theme::run_indicator_style(*indicator)),
        )));
    }
    if let Some((label, badge_style)) = bot_watch_badge(badges, &ticket.key) {
        lines.push(Line::from(Span::styled(label, style.patch(badge_style))));
    }
    if let Some(entry) = badges.cleanup_status.get(&ticket.key) {
        lines.push(Line::from(Span::styled(
            theme::cleanup_indicator_label(entry.indicator),
            style.patch(theme::cleanup_indicator_style(entry.indicator)),
        )));
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
/// capped summary plus top/bottom border rows, plus one more row each for
/// the assignee line (when `show_assignee` is set), the audit badge line
/// (when `ticket.key` has an entry in `badges.audit_status`), the lane-run
/// badge line (when it has an entry in `badges.lane_run_status`), the
/// bot-watch badge line (when [`bot_watch_badge`] yields one), and the
/// bugbot-cleanup badge line (when it has an entry in
/// `badges.cleanup_status`) -- every badge line can render on the same card at
/// once.
fn card_height(
    ticket: &TicketSummary,
    content_width: usize,
    show_assignee: bool,
    badges: &BoardBadges,
) -> u16 {
    let lines = wrapped_summary(&ticket.summary, content_width).len() as u16;
    let assignee_row = if show_assignee { 1 } else { 0 };
    let audit_row = if badges.audit_status.contains_key(&ticket.key) {
        1
    } else {
        0
    };
    let lane_run_row = if badges.lane_run_status.contains_key(&ticket.key) {
        1
    } else {
        0
    };
    let bot_watch_row = if bot_watch_badge(badges, &ticket.key).is_some() {
        1
    } else {
        0
    };
    let cleanup_row = if badges.cleanup_status.contains_key(&ticket.key) {
        1
    } else {
        0
    };
    lines + assignee_row + audit_row + lane_run_row + bot_watch_row + cleanup_row + CARD_BORDER_ROWS
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

/// [`Screen::Retro`]: a flat list of shipped tickets awaiting a verdict, one
/// per row, showing each ticket's key, its latest lane run's cost and model
/// mix (or `no run` when it never had one -- see
/// [`crate::tui::app::RetroRunInfo`]'s doc comment for why that's kept
/// visually distinct from a `$0.00` run), and its summary. An empty list
/// renders as a plain "caught up" message rather than an empty bordered box,
/// since an empty retro board is the steady state this screen is meant to
/// reach, not an error or a loading state.
fn draw_retro_list(frame: &mut Frame, app: &App, area: Rect) {
    if app.retro_tickets.is_empty() {
        let paragraph = Paragraph::new("Nothing awaiting a retro verdict -- you're caught up.")
            .block(Block::default().borders(Borders::ALL).title("Retro"));
        frame.render_widget(paragraph, area);
        return;
    }

    let items: Vec<ListItem> = app
        .retro_tickets
        .iter()
        .map(|ticket| {
            let key_span = Span::styled(format!(" {}", ticket.key), theme::CARD_KEY);
            let run_span = match &ticket.run {
                None => Span::styled("  no run".to_string(), theme::DIM),
                Some(run) => {
                    let cost = run
                        .cost_usd
                        .map(|cost| format!("${cost:.2}"))
                        .unwrap_or_else(|| "pending".to_string());
                    let model = run.model_summary.as_deref().unwrap_or("no model usage");
                    Span::raw(format!("  {cost}  {model}"))
                }
            };
            let summary_span = Span::raw(format!("  {}", ticket.summary));
            ListItem::new(Line::from(vec![key_span, run_span, summary_span]))
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("Retro"))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    let mut state = ListState::default();
    state.select(Some(app.retro_selected.min(app.retro_tickets.len() - 1)));
    frame.render_stateful_widget(list, area, &mut state);
}

/// The display label for a [`RetroSeverity`], for
/// [`draw_retro_severity_picker`]'s option list.
fn retro_severity_label(severity: RetroSeverity) -> &'static str {
    match severity {
        RetroSeverity::Minor => "Minor",
        RetroSeverity::Major => "Major",
        RetroSeverity::Critical => "Critical",
    }
}

/// A centered floating window listing [`RETRO_SEVERITIES`], for the defect
/// flow's severity step ([`Msg::RetroDefectStart`]). Structurally identical
/// to [`draw_lane_picker`] -- synchronous, fixed options, no "active" marker.
///
/// [`Msg::RetroDefectStart`]: crate::tui::app::Msg::RetroDefectStart
fn draw_retro_severity_picker(frame: &mut Frame, app: &App) {
    let area = centered_rect(40, 30, frame.area());
    frame.render_widget(Clear, area);

    let items: Vec<ListItem> = RETRO_SEVERITIES
        .iter()
        .map(|severity| ListItem::new(format!("  {}", retro_severity_label(*severity))))
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(bold_title("Severity")),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    let mut state = ListState::default();
    state.select(Some(
        app.retro_severity_selected.min(RETRO_SEVERITIES.len() - 1),
    ));
    frame.render_stateful_widget(list, area, &mut state);
}

/// A centered floating window for the defect flow's optional note step
/// ([`Msg::RetroSeverityPickerSelect`]): a single line of free text built up
/// character by character (see `Msg::RetroNoteChar`/`Backspace`), `Enter` to
/// submit (empty is fine -- no note gets recorded), `Esc` to cancel the whole
/// defect flow.
///
/// [`Msg::RetroSeverityPickerSelect`]: crate::tui::app::Msg::RetroSeverityPickerSelect
fn draw_retro_note_entry(frame: &mut Frame, app: &App) {
    let area = centered_rect(50, 30, frame.area());
    frame.render_widget(Clear, area);

    let lines = vec![
        Line::from(app.retro_note_draft.clone()),
        Line::from(""),
        Line::from(Span::styled(
            "Enter to submit (blank = no note), Esc to cancel",
            theme::DIM,
        )),
    ];
    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(bold_title("Note (optional)")),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
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
/// [`STALE_HEARTBEAT_SECS`] gets a trailing red `!` on its ticket line. A
/// card whose run is awaiting input (see
/// [`crate::runs::is_awaiting_input`]) gets a trailing bold-yellow
/// ` waiting` badge on its lane line.
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
    if card.awaiting_input {
        lane_spans.push(Span::styled(" waiting", style.patch(theme::AWAITING_INPUT)));
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

/// Fixed height (in rows) of the run-detail window's middle row -- the
/// side-by-side Usage/Checklist panels. Each panel's border consumes the top
/// and bottom row, leaving `MIDDLE_ROW_HEIGHT - 2` for content before
/// [`truncate_lines`] kicks in.
const MIDDLE_ROW_HEIGHT: u16 = 8;

/// A centered floating window (~90% wide, ~80% tall) showing the selected
/// run's full detail and event timeline, drawn over the runs board (or, per
/// `docs/plans/board-run-detail.md`, the ticket board).
///
/// While `app.run_detail` is still loading (`None`), shows a placeholder
/// instead. Otherwise the window is a header grid (identity/timing/cost
/// facts, see [`draw_header_grid`]), a middle row of Usage/Checklist panels
/// (see [`draw_middle_row`]), and an events panel (see
/// [`draw_events_panel`]) that alone scrolls with `app.run_detail_scroll` --
/// the header and middle row are bounded summaries, the timeline isn't.
fn draw_run_detail_window(frame: &mut Frame, app: &App) {
    let area = centered_rect(90, 80, frame.area());
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

    // fg-only accent (see theme.rs's doctrine): the run's status color on
    // both the outer border and its title, so the window carries the same
    // at-a-glance signal as the card badges that led here.
    let accent = theme::run_status_style(detail.status);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(accent)
        .title(Line::from(Span::styled(
            run_detail_title(detail),
            theme::BOLD.patch(accent),
        )));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_grid_height(detail)),
            Constraint::Length(MIDDLE_ROW_HEIGHT),
            Constraint::Min(0),
        ])
        .split(inner);

    draw_header_grid(frame, rows[0], detail);
    draw_middle_row(frame, rows[1], detail);
    draw_events_panel(frame, rows[2], detail, app.run_detail_scroll);
}

/// The run-detail window's title: `Run {id}: {ticket}` for the common `lane`
/// kind, `Run {id}: {ticket} ({kind})` for everything else (`audit`,
/// `create`, `review-watch`, ...).
fn run_detail_title(detail: &crate::tui::app::RunDetail) -> String {
    if detail.kind == "lane" {
        format!("Run {}: {}", detail.id, detail.ticket)
    } else {
        format!("Run {}: {} ({})", detail.id, detail.ticket, detail.kind)
    }
}

/// One header-grid line: `"{label}: "` dim, `value` styled `value_style`.
fn label_value_line(label: &str, value: impl Into<String>, value_style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label}: "), theme::DIM),
        Span::styled(value.into(), value_style),
    ])
}

/// The header grid's identity column: lane, kind (colored via
/// [`theme::kind_style`]), and status (colored via
/// [`theme::run_status_style`], with the `(waiting)` marker computed exactly
/// as the pre-redesign window did).
fn identity_lines(detail: &crate::tui::app::RunDetail) -> Vec<Line<'static>> {
    let awaiting_input = crate::runs::is_awaiting_input(
        detail.status,
        detail.events.last().map(|event| event.kind.as_str()),
    );
    let mut status_spans = vec![
        Span::styled("status: ", theme::DIM),
        Span::styled(
            detail.status.as_str().to_string(),
            theme::run_status_style(detail.status),
        ),
    ];
    if awaiting_input {
        status_spans.push(Span::styled(" (waiting)", theme::AWAITING_INPUT));
    }
    vec![
        label_value_line("lane", detail.lane.clone(), Style::default()),
        label_value_line("kind", detail.kind.clone(), theme::kind_style(&detail.kind)),
        Line::from(status_spans),
    ]
}

/// The header grid's timing column: started (always present), ended and
/// turns (only when known).
fn timing_lines(detail: &crate::tui::app::RunDetail) -> Vec<Line<'static>> {
    let mut lines = vec![label_value_line(
        "started",
        detail.started_at.clone(),
        Style::default(),
    )];
    if let Some(ended_at) = &detail.ended_at {
        lines.push(label_value_line(
            "ended",
            ended_at.clone(),
            Style::default(),
        ));
    }
    if let Some(turns) = detail.num_turns {
        lines.push(label_value_line(
            "turns",
            turns.to_string(),
            Style::default(),
        ));
    }
    lines
}

/// The header grid's cost/process column: cost, pid, session -- all
/// optional, omitted when absent.
fn cost_process_lines(detail: &crate::tui::app::RunDetail) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(cost) = detail.cost_usd {
        lines.push(label_value_line(
            "cost",
            format!("${cost:.2}"),
            Style::default(),
        ));
    }
    if let Some(pid) = detail.pid {
        lines.push(label_value_line("pid", pid.to_string(), Style::default()));
    }
    if let Some(session_id) = &detail.session_id {
        lines.push(label_value_line(
            "session",
            session_id.clone(),
            Style::default(),
        ));
    }
    lines
}

/// The header grid's full-width lines below the three columns: worktree
/// (always present), branch/pr/blocker (only when present) -- paths too long
/// for a column.
fn full_width_lines(detail: &crate::tui::app::RunDetail) -> Vec<Line<'static>> {
    let mut lines = vec![label_value_line(
        "worktree",
        detail.worktree.clone(),
        Style::default(),
    )];
    if let Some(branch) = &detail.branch {
        lines.push(label_value_line("branch", branch.clone(), Style::default()));
    }
    if let Some(pr_url) = &detail.pr_url {
        lines.push(label_value_line("pr", pr_url.clone(), Style::default()));
    }
    if let Some(blocker) = &detail.blocker {
        lines.push(label_value_line(
            "blocker",
            blocker.clone(),
            Style::default(),
        ));
    }
    lines
}

/// The header grid's total height: its tallest column plus the full-width
/// lines beneath it.
fn header_grid_height(detail: &crate::tui::app::RunDetail) -> u16 {
    let max_col = identity_lines(detail)
        .len()
        .max(timing_lines(detail).len())
        .max(cost_process_lines(detail).len());
    (max_col + full_width_lines(detail).len()) as u16
}

/// Draws the run-detail window's header: three side-by-side label-value
/// columns (identity, timing, cost/process; see [`identity_lines`],
/// [`timing_lines`], [`cost_process_lines`]) over full-width lines (see
/// [`full_width_lines`]).
fn draw_header_grid(frame: &mut Frame, area: Rect, detail: &crate::tui::app::RunDetail) {
    if area.height == 0 {
        return;
    }
    let identity = identity_lines(detail);
    let timing = timing_lines(detail);
    let cost = cost_process_lines(detail);
    let full_width = full_width_lines(detail);
    let grid_height = identity.len().max(timing.len()).max(cost.len()) as u16;

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(grid_height),
            Constraint::Length(full_width.len() as u16),
        ])
        .split(area);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Ratio(1, 3),
            Constraint::Ratio(1, 3),
            Constraint::Ratio(1, 3),
        ])
        .split(rows[0]);

    frame.render_widget(Paragraph::new(identity), cols[0]);
    frame.render_widget(Paragraph::new(timing), cols[1]);
    frame.render_widget(Paragraph::new(cost), cols[2]);
    frame.render_widget(Paragraph::new(full_width), rows[1]);
}

/// Truncates `lines` to `max_height` rows, replacing the last row with a dim
/// `"... +N more"` marker (ASCII dots, matching the house no-emoji rule) when
/// content overflows. A no-op when `lines` already fits or `max_height` is
/// `0` (nothing would be visible either way).
fn truncate_lines(lines: Vec<Line<'static>>, max_height: usize) -> Vec<Line<'static>> {
    let total = lines.len();
    if max_height == 0 || total <= max_height {
        return lines;
    }
    let keep = max_height - 1;
    let mut out: Vec<Line<'static>> = lines.into_iter().take(keep).collect();
    out.push(Line::from(Span::styled(
        format!("... +{} more", total - keep),
        theme::DIM,
    )));
    out
}

/// The Usage panel's content: model usage lines (if any), then agent usage
/// lines under an inline dim "Agent usage" sub-header, then the tool-counts
/// line (dim) if present. A dim placeholder when none of the three apply, so
/// the panel never collapses across refreshes.
fn usage_panel_lines(detail: &crate::tui::app::RunDetail) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(usage) = &detail.model_usage {
        for line in &usage.lines {
            lines.push(Line::from(line.clone()));
        }
    }
    if !detail.agent_usage.is_empty() {
        lines.push(Line::from(Span::styled("Agent usage", theme::DIM)));
        for line in &detail.agent_usage {
            lines.push(Line::from(line.clone()));
        }
    }
    if let Some(tools_line) = crate::runs::format_tool_counts(&detail.tool_counts) {
        lines.push(Line::from(Span::styled(tools_line, theme::DIM)));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled("no usage yet", theme::DIM)));
    }
    lines
}

/// Draws the Usage panel: bordered, titled with the model usage's label
/// (`"Model usage"` / `"Model usage (live)"`, see [`RunModelUsage`](
/// crate::tui::app::RunModelUsage)) when known, else the generic `"Usage"`.
fn draw_usage_panel(frame: &mut Frame, area: Rect, detail: &crate::tui::app::RunDetail) {
    let title = detail
        .model_usage
        .as_ref()
        .map(|usage| usage.label)
        .unwrap_or("Usage");
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(title, theme::SECTION_HEADER));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = truncate_lines(usage_panel_lines(detail), inner.height as usize);
    frame.render_widget(Paragraph::new(lines), inner);
}

/// The Checklist panel's content: `[x]`/`[ ]` items (green/dim, see
/// [`theme::CHECKLIST_DONE`]/[`theme::CHECKLIST_PENDING`]), or a dim
/// placeholder when there's no checklist or it's empty.
fn checklist_panel_lines(detail: &crate::tui::app::RunDetail) -> Vec<Line<'static>> {
    match &detail.checklist {
        Some(checklist) if !checklist.items.is_empty() => checklist
            .items
            .iter()
            .map(|item| {
                let (marker, item_style) = if item.done {
                    ("[x]", theme::CHECKLIST_DONE)
                } else {
                    ("[ ]", theme::CHECKLIST_PENDING)
                };
                Line::from(Span::styled(format!("{marker} {}", item.text), item_style))
            })
            .collect(),
        _ => vec![Line::from(Span::styled("no checklist", theme::DIM))],
    }
}

/// Draws the Checklist panel: bordered, titled `"Checklist {done}/{total}"`
/// when a checklist exists, else the bare `"Checklist"`.
fn draw_checklist_panel(frame: &mut Frame, area: Rect, detail: &crate::tui::app::RunDetail) {
    let title = match &detail.checklist {
        Some(checklist) => format!(
            "Checklist {}/{}",
            checklist.done_count(),
            checklist.items.len()
        ),
        None => "Checklist".to_string(),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(title, theme::SECTION_HEADER));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = truncate_lines(checklist_panel_lines(detail), inner.height as usize);
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Draws the run-detail window's middle row: the Usage and Checklist panels
/// side by side, ~50/50.
fn draw_middle_row(frame: &mut Frame, area: Rect, detail: &crate::tui::app::RunDetail) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    draw_usage_panel(frame, cols[0], detail);
    draw_checklist_panel(frame, cols[1], detail);
}

/// One event timeline line: timestamp (dim), kind (accented via
/// [`theme::event_kind_style`]), then the friendly detail from
/// [`crate::runs::format_event_detail`] (falling back to the raw detail
/// payload, then to nothing) -- the same fallback chain the pre-redesign
/// window used, just with the kind pulled into its own styled span.
fn event_line(event: &crate::tui::app::RunDetailEvent) -> Line<'static> {
    let detail_text = crate::runs::format_event_detail(&event.kind, event.detail.as_deref())
        .or_else(|| event.detail.clone());
    let mut spans = vec![
        Span::styled(event.at.clone(), theme::DIM),
        Span::raw("  "),
        Span::styled(event.kind.clone(), theme::event_kind_style(&event.kind)),
    ];
    if let Some(text) = detail_text {
        spans.push(Span::raw(format!("  {text}")));
    }
    Line::from(spans)
}

/// The events panel's content: the timeline, newest first, or a dim
/// `"(no events)"` placeholder.
fn event_lines(detail: &crate::tui::app::RunDetail) -> Vec<Line<'static>> {
    if detail.events.is_empty() {
        return vec![Line::from(Span::styled("(no events)", theme::DIM))];
    }
    detail.events.iter().rev().map(event_line).collect()
}

/// Draws the run-detail window's Events panel: bordered, titled "Events",
/// scrolled by `scroll` -- the only part of the window that scrolls, per
/// `docs/plans/board-run-detail.md`'s "Overlay redesign" decision.
fn draw_events_panel(
    frame: &mut Frame,
    area: Rect,
    detail: &crate::tui::app::RunDetail,
    scroll: u16,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled("Events", theme::SECTION_HEADER));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let paragraph = Paragraph::new(event_lines(detail)).scroll((scroll, 0));
    frame.render_widget(paragraph, inner);
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
        Some(ticket) => {
            let mut lines = vec![
                Line::from(vec![
                    Span::styled("Status: ", theme::SECTION_HEADER),
                    Span::styled(
                        ticket.status.clone(),
                        theme::ticket_status_style(&ticket.status_category),
                    ),
                ]),
                Line::from(Span::styled(ticket.url.clone(), theme::DIM)),
                Line::from(""),
            ];
            // `split('\n')`, not `.lines()`: a trailing blank line in the
            // description (e.g. a trailing hardBreak) should still render as
            // an empty row rather than being silently dropped.
            lines.extend(
                ticket
                    .description
                    .split('\n')
                    .map(|segment| Line::from(segment.to_string())),
            );
            lines
        }
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
    let area = centered_rect(60, 75, frame.area());
    let lines = vec![
        Line::from("j / Down    move down"),
        Line::from("k / Up      move up"),
        Line::from("h / Left    previous column"),
        Line::from("l / Right   next column"),
        Line::from("Enter       open / apply"),
        Line::from("Esc / q     back (quits from the board)"),
        Line::from("r           refresh tickets (inert mid-grab in priority view)"),
        Line::from("o           open in browser (picks Jira/GitHub if a PR exists, board only)"),
        Line::from("O           open Jira directly, no PR lookup"),
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

/// A centered floating window listing the board's repo-compatible
/// `[work.lanes]` names (`app.lane_names`, in `BTreeMap` order), for
/// [`Msg::LaneRunAction`]'s lane picker. Unlike [`draw_filter_picker`], the
/// data is synchronous (no lazy fetch, so no loading/error line) and there's
/// no "active lane" to mark -- only the highlighted row, via the list's own
/// `highlight_style`. The title notes how many lanes were hidden for a
/// backend mismatch (`app.hidden_lane_count`), if any -- see GitHub issue
/// #5 phase 2: `docs/plans/issue-5-lane-backend-routing.md`.
///
/// [`Msg::LaneRunAction`]: crate::tui::app::Msg::LaneRunAction
fn draw_lane_picker(frame: &mut Frame, app: &App) {
    let area = centered_rect(50, 50, frame.area());
    frame.render_widget(Clear, area);

    let items: Vec<ListItem> = app
        .lane_names
        .iter()
        .map(|lane| ListItem::new(format!("  {lane}")))
        .collect();

    let title = if app.hidden_lane_count > 0 {
        format!("Lane ({} hidden: backend mismatch)", app.hidden_lane_count)
    } else {
        "Lane".to_string()
    };
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(bold_title(title)),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    let mut state = ListState::default();
    if !app.lane_names.is_empty() {
        state.select(Some(app.lane_picker_selected.min(app.lane_names.len() - 1)));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

/// A centered floating window listing `app.browser_picker_options`'s two
/// choices (Jira, then the resolved GitHub PR), for [`Msg::OpenBrowserAction`]'s
/// browser picker. Structurally identical to [`draw_lane_picker`] -- the data
/// is synchronous (built once by
/// [`crate::tui::app::browser_options_resolved`], no lazy fetch and so no
/// loading/error line) and there's no "active" option to mark, only the
/// highlighted row via the list's own `highlight_style`.
///
/// [`Msg::OpenBrowserAction`]: crate::tui::app::Msg::OpenBrowserAction
fn draw_browser_picker(frame: &mut Frame, app: &App) {
    let area = centered_rect(50, 50, frame.area());
    frame.render_widget(Clear, area);

    let items: Vec<ListItem> = app
        .browser_picker_options
        .iter()
        .map(|option| ListItem::new(format!("  {}", option.label())))
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(bold_title("Open in browser")),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    let mut state = ListState::default();
    if !app.browser_picker_options.is_empty() {
        state.select(Some(
            app.browser_picker_selected
                .min(app.browser_picker_options.len() - 1),
        ));
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
    use crate::runs::RetroSeverity;
    use crate::ticketing::types::{Status, StatusCategory, Transition};
    use crate::tui::app::BrowserPickerOption;
    use crate::tui::app::{
        Column, RETRO_SEVERITIES, RetroRow, RetroRunInfo, TicketSummary, group_into_columns,
    };
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
            "o browser",
            "O jira",
            "f filter",
            "p priority",
            "a audit",
            "s session",
            "w work",
            "b bots",
            "v view run",
            "L logs",
            "V vdiff",
            "F fix",
            "R retro",
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
    fn board_column_title_carries_its_status_category_color() {
        let app = three_column_app();
        let buffer = render(&app);
        let cell = cell_at(&buffer, "In Progress (1)").expect("column title renders");
        assert_eq!(
            Some(cell.fg),
            theme::ticket_status_style("indeterminate").fg
        );
    }

    #[test]
    fn empty_column_title_falls_back_to_plain_column_title_style() {
        let column = Column {
            title: "Backlog".to_string(),
            tickets: vec![],
        };
        assert_eq!(column_title_style(&column), theme::COLUMN_TITLE);
    }

    #[test]
    fn assignee_line_renders_dim() {
        let app = App {
            columns: group_into_columns(vec![ticket("PROJ-1")], &[]),
            filter: AssigneeFilter::Everyone,
            ..App::new()
        };
        let buffer = render(&app);
        let cell = cell_at(&buffer, "Assignee: Unassigned").expect("assignee line renders");
        assert_eq!(Some(cell.fg), theme::DIM.fg);
    }

    #[test]
    fn detail_overlay_status_value_carries_its_status_category_color() {
        let app = App {
            columns: group_into_columns(vec![ticket("PROJ-1")], &[]),
            screen: Screen::Detail,
            ..App::new()
        };
        let buffer = render(&app);
        // Locate the "Status: " label, then check the color of the value
        // cell right after it -- searching for "In Progress" directly would
        // ambiguously match the board's (same-colored) column title, which
        // is still visible behind the overlay.
        let (x, y) = cell_pos(&buffer, "Status: ").expect("status label renders");
        let value_cell = &buffer[(x + "Status: ".chars().count() as u16, y)];
        assert_eq!(
            Some(value_cell.fg),
            theme::ticket_status_style("indeterminate").fg
        );
    }

    #[test]
    fn detail_overlay_url_line_renders_dim() {
        let app = App {
            columns: group_into_columns(vec![ticket("PROJ-1")], &[]),
            screen: Screen::Detail,
            ..App::new()
        };
        let buffer = render(&app);
        let cell = cell_at(&buffer, "https://example.atlassian.net/browse/PROJ-1")
            .expect("url line renders");
        assert_eq!(Some(cell.fg), theme::DIM.fg);
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
    fn detail_overlay_renders_description_paragraphs_on_separate_rows() {
        let app = App {
            columns: group_into_columns(
                vec![TicketSummary {
                    description: "first paragraph\n\nsecond paragraph".to_string(),
                    ..ticket("PROJ-1")
                }],
                &[],
            ),
            screen: Screen::Detail,
            ..App::new()
        };
        let buffer = render(&app);
        let (_, first_y) = cell_pos(&buffer, "first paragraph").expect("first paragraph renders");
        let (_, second_y) =
            cell_pos(&buffer, "second paragraph").expect("second paragraph renders");
        assert_ne!(
            first_y, second_y,
            "paragraphs must render on different rows, not collapse into one blob"
        );
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

    /// Find the `(x, y)` position at which `needle` starts rendering
    /// (reading left-to-right, top-to-bottom), verifying the full string
    /// actually matches from that point rather than just its first
    /// character.
    fn cell_pos(buffer: &ratatui::buffer::Buffer, needle: &str) -> Option<(u16, u16)> {
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
                        return Some((x, y));
                    }
                }
            }
        }
        None
    }

    /// The cell at which `needle` starts rendering; see [`cell_pos`].
    fn cell_at<'a>(
        buffer: &'a ratatui::buffer::Buffer,
        needle: &str,
    ) -> Option<&'a ratatui::buffer::Cell> {
        let (x, y) = cell_pos(buffer, needle)?;
        Some(&buffer[(x, y)])
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
    fn card_with_audit_status_renders_the_badge_styled() {
        use crate::tui::app::AuditIndicator;

        let mut app = App {
            columns: group_into_columns(vec![ticket("PROJ-1")], &[]),
            ..App::new()
        };
        app.audit_status.insert(
            "PROJ-1".to_string(),
            AuditStatusEntry {
                indicator: AuditIndicator::Waiting,
                window_live: true,
            },
        );
        let buffer = render_with_size(&app, 80, 24);
        let text = buffer_text(&buffer);
        assert!(text.contains("audit: waiting"));
        let cell = cell_at(&buffer, "audit: waiting").expect("audit badge renders");
        assert_eq!(Some(cell.fg), theme::AWAITING_INPUT.fg);
        assert!(cell.modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn card_with_no_audit_status_renders_no_badge() {
        let app = App {
            columns: group_into_columns(vec![ticket("PROJ-1")], &[]),
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        assert!(!text.contains("audit:"));
    }

    #[test]
    fn card_with_lane_run_status_renders_the_badge_styled() {
        use crate::tui::app::RunIndicator;

        let mut app = App {
            columns: group_into_columns(vec![ticket("PROJ-1")], &[]),
            ..App::new()
        };
        app.lane_run_status
            .insert("PROJ-1".to_string(), RunIndicator::Waiting);
        let buffer = render_with_size(&app, 80, 24);
        let text = buffer_text(&buffer);
        assert!(text.contains("run: waiting"));
        let cell = cell_at(&buffer, "run: waiting").expect("lane run badge renders");
        assert_eq!(Some(cell.fg), theme::AWAITING_INPUT.fg);
        assert!(cell.modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn card_with_no_lane_run_status_renders_no_run_badge() {
        let app = App {
            columns: group_into_columns(vec![ticket("PROJ-1")], &[]),
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        assert!(!text.contains("run:"));
    }

    #[test]
    fn card_with_both_audit_and_lane_run_status_renders_both_badges() {
        use crate::tui::app::{AuditIndicator, RunIndicator};

        let mut app = App {
            columns: group_into_columns(vec![ticket("PROJ-1")], &[]),
            ..App::new()
        };
        app.audit_status.insert(
            "PROJ-1".to_string(),
            AuditStatusEntry {
                indicator: AuditIndicator::Running,
                window_live: true,
            },
        );
        app.lane_run_status
            .insert("PROJ-1".to_string(), RunIndicator::Done);
        let text = buffer_text(&render_with_size(&app, 80, 24));
        assert!(text.contains("audit: running"));
        assert!(text.contains("run: done"));
    }

    #[test]
    fn selected_card_with_lane_run_badge_still_carries_reversed_modifier() {
        use crate::tui::app::RunIndicator;

        let mut app = App {
            columns: group_into_columns(vec![ticket("PROJ-1")], &[]),
            selected_col: 0,
            selected_row: 0,
            ..App::new()
        };
        app.lane_run_status
            .insert("PROJ-1".to_string(), RunIndicator::Running);
        let buffer = render_with_size(&app, 80, 24);
        let modifier = modifier_at(&buffer, "run: running");
        assert!(
            modifier.contains(Modifier::REVERSED),
            "selection contract (REVERSED) must survive on a badged card"
        );
    }

    #[test]
    fn selected_card_with_audit_badge_still_carries_reversed_modifier() {
        use crate::tui::app::AuditIndicator;

        let mut app = App {
            columns: group_into_columns(vec![ticket("PROJ-1")], &[]),
            selected_col: 0,
            selected_row: 0,
            ..App::new()
        };
        app.audit_status.insert(
            "PROJ-1".to_string(),
            AuditStatusEntry {
                indicator: AuditIndicator::Running,
                window_live: true,
            },
        );
        let buffer = render_with_size(&app, 80, 24);
        let modifier = modifier_at(&buffer, "audit: running");
        assert!(
            modifier.contains(Modifier::REVERSED),
            "selection contract (REVERSED) must survive on a badged card"
        );
    }

    #[test]
    fn card_with_bot_watch_status_renders_the_badge_styled() {
        use crate::tui::app::BotWatchIndicator;

        let mut app = App {
            columns: group_into_columns(vec![ticket("PROJ-1")], &[]),
            ..App::new()
        };
        app.bot_watch_status
            .insert("PROJ-1".to_string(), BotWatchIndicator::Watching);
        let buffer = render_with_size(&app, 80, 24);
        let cell = cell_at(&buffer, "bots: watching").expect("bots badge renders");
        assert_eq!(
            Some(cell.fg),
            theme::bot_watch_indicator_style(BotWatchIndicator::Watching).fg
        );
    }

    #[test]
    fn card_with_ready_bot_watch_status_renders_the_loud_accent() {
        use crate::tui::app::BotWatchIndicator;

        let mut app = App {
            columns: group_into_columns(vec![ticket("PROJ-1")], &[]),
            ..App::new()
        };
        app.bot_watch_status
            .insert("PROJ-1".to_string(), BotWatchIndicator::Ready);
        let buffer = render_with_size(&app, 80, 24);
        let cell = cell_at(&buffer, "bots: ready").expect("bots badge renders");
        assert_eq!(Some(cell.fg), theme::AWAITING_INPUT.fg);
        assert!(cell.modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn card_with_no_bot_watch_status_renders_no_bots_badge() {
        let app = App {
            columns: group_into_columns(vec![ticket("PROJ-1")], &[]),
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        assert!(!text.contains("bots:"));
    }

    #[test]
    fn card_with_a_pending_bot_watch_launch_renders_the_starting_overlay() {
        let mut app = App {
            columns: group_into_columns(vec![ticket("PROJ-1")], &[]),
            ..App::new()
        };
        app.pending_bot_watch_launches.insert("PROJ-1".to_string());
        let buffer = render_with_size(&app, 80, 24);
        let cell = cell_at(&buffer, theme::BOT_WATCH_STARTING_LABEL)
            .expect("starting overlay badge renders");
        assert_eq!(Some(cell.fg), theme::DIM.fg);
    }

    #[test]
    fn a_loaded_bot_watch_run_wins_over_a_pending_launch_overlay() {
        use crate::tui::app::BotWatchIndicator;

        let mut app = App {
            columns: group_into_columns(vec![ticket("PROJ-1")], &[]),
            ..App::new()
        };
        app.pending_bot_watch_launches.insert("PROJ-1".to_string());
        app.bot_watch_status
            .insert("PROJ-1".to_string(), BotWatchIndicator::Watching);
        let text = buffer_text(&render_with_size(&app, 80, 24));
        assert!(text.contains("bots: watching"));
        assert!(!text.contains("bots: starting"));
    }

    #[test]
    fn card_with_cleanup_status_renders_the_clean_badge_styled() {
        use crate::tui::app::AuditIndicator;

        let mut app = App {
            columns: group_into_columns(vec![ticket("PROJ-1")], &[]),
            ..App::new()
        };
        app.cleanup_status.insert(
            "PROJ-1".to_string(),
            AuditStatusEntry {
                indicator: AuditIndicator::Running,
                window_live: true,
            },
        );
        let buffer = render_with_size(&app, 80, 24);
        let cell = cell_at(&buffer, "clean: running").expect("clean badge renders");
        assert_eq!(
            Some(cell.fg),
            theme::cleanup_indicator_style(AuditIndicator::Running).fg
        );
    }

    #[test]
    fn card_with_no_cleanup_status_renders_no_clean_badge() {
        let app = App {
            columns: group_into_columns(vec![ticket("PROJ-1")], &[]),
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        assert!(!text.contains("clean:"));
    }

    #[test]
    fn card_can_render_every_badge_family_at_once() {
        use crate::tui::app::{AuditIndicator, BotWatchIndicator, RunIndicator};

        let mut app = App {
            columns: group_into_columns(vec![ticket("PROJ-1")], &[]),
            ..App::new()
        };
        app.audit_status.insert(
            "PROJ-1".to_string(),
            AuditStatusEntry {
                indicator: AuditIndicator::Done,
                window_live: true,
            },
        );
        app.lane_run_status
            .insert("PROJ-1".to_string(), RunIndicator::Done);
        app.bot_watch_status
            .insert("PROJ-1".to_string(), BotWatchIndicator::Ready);
        app.cleanup_status.insert(
            "PROJ-1".to_string(),
            AuditStatusEntry {
                indicator: AuditIndicator::Waiting,
                window_live: true,
            },
        );
        let text = buffer_text(&render_with_size(&app, 80, 24));
        assert!(text.contains("audit: done"));
        assert!(text.contains("run: done"));
        assert!(text.contains("bots: ready"));
        assert!(text.contains("clean: waiting"));
    }

    #[test]
    fn selected_card_with_bots_badge_still_carries_reversed_modifier() {
        use crate::tui::app::BotWatchIndicator;

        let mut app = App {
            columns: group_into_columns(vec![ticket("PROJ-1")], &[]),
            selected_col: 0,
            selected_row: 0,
            ..App::new()
        };
        app.bot_watch_status
            .insert("PROJ-1".to_string(), BotWatchIndicator::Watching);
        let buffer = render_with_size(&app, 80, 24);
        let modifier = modifier_at(&buffer, "bots: watching");
        assert!(
            modifier.contains(Modifier::REVERSED),
            "selection contract (REVERSED) must survive on a badged card"
        );
    }

    #[test]
    fn board_hint_documents_the_bots_key() {
        assert!(hint_for(Screen::Board, false).contains("b bots"));
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

    fn jira_user(account_id: &str, display_name: &str) -> crate::ticketing::types::JiraUser {
        crate::ticketing::types::JiraUser {
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
    fn draws_lane_picker_overlay_with_lane_names() {
        let app = App {
            show_lane_picker: true,
            lane_names: vec!["backend".to_string(), "frontend".to_string()],
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        assert!(text.contains("Lane"));
        assert!(text.contains("backend"));
        assert!(text.contains("frontend"));
    }

    #[test]
    fn draws_lane_picker_highlights_the_selected_lane() {
        let app = App {
            show_lane_picker: true,
            lane_picker_selected: 1,
            lane_names: vec!["backend".to_string(), "frontend".to_string()],
            ..App::new()
        };
        let buffer = render(&app);
        let modifier = modifier_at(&buffer, "frontend");
        assert!(modifier.contains(Modifier::REVERSED));
        let modifier = modifier_at(&buffer, "backend");
        assert!(!modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn draws_browser_picker_overlay_with_option_labels() {
        let app = App {
            show_browser_picker: true,
            browser_picker_options: vec![
                BrowserPickerOption::Jira {
                    key: "PROJ-1".to_string(),
                    url: "https://example.atlassian.net/browse/PROJ-1".to_string(),
                },
                BrowserPickerOption::GitHub {
                    number: 42,
                    url: "https://github.com/example/repo/pull/42".to_string(),
                },
            ],
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        assert!(text.contains("Open in browser"));
        assert!(text.contains("Jira (PROJ-1)"));
        assert!(text.contains("GitHub (#42)"));
    }

    #[test]
    fn draws_browser_picker_highlights_the_selected_option() {
        let app = App {
            show_browser_picker: true,
            browser_picker_selected: 1,
            browser_picker_options: vec![
                BrowserPickerOption::Jira {
                    key: "PROJ-1".to_string(),
                    url: "https://example.atlassian.net/browse/PROJ-1".to_string(),
                },
                BrowserPickerOption::GitHub {
                    number: 42,
                    url: "https://github.com/example/repo/pull/42".to_string(),
                },
            ],
            ..App::new()
        };
        let buffer = render(&app);
        let modifier = modifier_at(&buffer, "GitHub (#42)");
        assert!(modifier.contains(Modifier::REVERSED));
        let modifier = modifier_at(&buffer, "Jira (PROJ-1)");
        assert!(!modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn rank_hint_documents_the_o_and_capital_o_split() {
        let hint = hint_for(Screen::Rank, false);
        assert!(hint.contains("o browser"));
        assert!(hint.contains("O jira"));
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

    fn retro_row(key: &str, run: Option<RetroRunInfo>) -> RetroRow {
        RetroRow {
            key: key.to_string(),
            summary: "Fix the thing".to_string(),
            url: format!("https://example.atlassian.net/browse/{key}"),
            run,
        }
    }

    #[test]
    fn draws_retro_list_with_run_shows_cost_and_model_mix() {
        let app = App {
            screen: Screen::Retro,
            retro_tickets: vec![retro_row(
                "PROJ-1",
                Some(RetroRunInfo {
                    cost_usd: Some(4.5),
                    model_summary: Some("fable-5 58.6k out".to_string()),
                }),
            )],
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        assert!(text.contains("PROJ-1"));
        assert!(text.contains("$4.50"));
        assert!(text.contains("fable-5 58.6k out"));
        assert!(text.contains("Fix the thing"));
    }

    #[test]
    fn draws_retro_list_with_no_run_shows_no_run_not_dollar_zero() {
        let app = App {
            screen: Screen::Retro,
            retro_tickets: vec![retro_row("PROJ-1", None)],
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        assert!(text.contains("no run"));
        assert!(!text.contains("$0.00"));
    }

    #[test]
    fn draws_retro_empty_list_shows_a_caught_up_message_not_a_blank_box() {
        let app = App {
            screen: Screen::Retro,
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        assert!(text.contains("Nothing awaiting a retro verdict"));
    }

    #[test]
    fn retro_screen_does_not_show_board_columns_behind_it() {
        let app = App {
            screen: Screen::Retro,
            retro_tickets: vec![retro_row("PROJ-1", None)],
            ..three_column_app()
        };
        let text = buffer_text(&render(&app));
        assert!(!text.contains("To Do (1)"));
        assert!(!text.contains("In Progress (1)"));
        assert!(!text.contains("Done (1)"));
    }

    #[test]
    fn retro_hint_documents_its_keys() {
        let hint = hint_for(Screen::Retro, false);
        for key in [
            "j/k",
            "d defect",
            "c clean",
            "r refresh",
            "Esc back",
            "q quit",
        ] {
            assert!(
                hint.contains(key),
                "retro hint should mention `{key}`: {hint}"
            );
        }
    }

    #[test]
    fn draws_retro_severity_picker_lists_every_severity() {
        let app = App {
            screen: Screen::Retro,
            show_retro_severity_picker: true,
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        assert!(text.contains("Severity"));
        assert!(text.contains("Minor"));
        assert!(text.contains("Major"));
        assert!(text.contains("Critical"));
    }

    #[test]
    fn retro_severity_label_covers_every_variant() {
        for severity in RETRO_SEVERITIES {
            assert!(!retro_severity_label(severity).is_empty());
        }
        assert_eq!(retro_severity_label(RetroSeverity::Minor), "Minor");
        assert_eq!(retro_severity_label(RetroSeverity::Major), "Major");
        assert_eq!(retro_severity_label(RetroSeverity::Critical), "Critical");
    }

    #[test]
    fn draws_retro_note_entry_shows_the_draft_and_instructions() {
        let app = App {
            screen: Screen::Retro,
            show_retro_note_entry: true,
            retro_note_draft: "it broke prod".to_string(),
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        assert!(text.contains("Note"));
        assert!(text.contains("it broke prod"));
        assert!(text.contains("Enter to submit"));
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
            awaiting_input: false,
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
        // Seven columns (Queued/Running/Blocked/Review/Done/Failed/
        // Interrupted) no longer fit the 80-column default without
        // truncating a title -- widen the terminal like the other
        // board-with-titles tests below. "Interrupted (0)" is the longest
        // title, so this one needs more room than the rest.
        let text = buffer_text(&render_with_size(&app, 140, 24));
        assert!(text.contains("Queued (0)"));
        assert!(text.contains("Running (0)"));
        assert!(text.contains("Blocked (0)"));
        assert!(text.contains("Review (0)"));
        assert!(text.contains("Done (0)"));
        assert!(text.contains("Failed (0)"));
        assert!(text.contains("Interrupted (0)"));
    }

    #[test]
    fn draws_runs_board_with_a_cards_ticket_and_lane_visible() {
        let app = runs_app(vec![run_card(1, "PROJ-1", "backend", RunStatus::Running)]);
        let text = buffer_text(&render_with_size(&app, 112, 24));
        assert!(text.contains("Running (1)"));
        assert!(text.contains("PROJ-1"));
        assert!(text.contains("backend"));
    }

    #[test]
    fn running_column_title_carries_its_status_color() {
        let app = runs_app(vec![run_card(1, "PROJ-1", "backend", RunStatus::Running)]);
        let buffer = render_with_size(&app, 112, 24);
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
    fn awaiting_run_card_shows_waiting_marker_styled_bold_yellow() {
        let card = RunCard {
            awaiting_input: true,
            ..run_card(1, "PROJ-1", "backend", RunStatus::Running)
        };
        let buffer = render_with_size(&runs_app(vec![card]), 160, 24);
        let text = buffer_text(&buffer);
        assert!(text.contains("waiting"));
        let cell = cell_at(&buffer, "waiting").expect("waiting marker renders");
        assert_eq!(Some(cell.fg), theme::AWAITING_INPUT.fg);
        assert!(cell.modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn non_awaiting_run_card_shows_no_waiting_marker() {
        let card = run_card(1, "PROJ-1", "backend", RunStatus::Running);
        assert!(!card.awaiting_input);
        let text = buffer_text(&render(&runs_app(vec![card])));
        assert!(!text.contains("waiting"));
    }

    #[test]
    fn selected_awaiting_run_card_still_carries_reversed_modifier() {
        let card = RunCard {
            awaiting_input: true,
            ..run_card(1, "PROJ-1", "backend", RunStatus::Running)
        };
        // RunStatus::Running is RUN_COLUMNS[1]; selecting its only row picks
        // this card.
        let app = App {
            runs_selected_col: 1,
            runs_selected_row: 0,
            ..runs_app(vec![card])
        };
        // A wide terminal so the lane name plus the appended waiting badge
        // renders unclipped, per non_lane_run_card_renders_its_kind_badge_styled.
        let buffer = render_with_size(&app, 160, 24);
        let modifier = modifier_at(&buffer, "waiting");
        assert!(
            modifier.contains(Modifier::REVERSED),
            "selection contract (REVERSED) must survive on a waiting card"
        );
    }

    #[test]
    fn run_detail_overlay_leaves_column_titles_visible() {
        let app = App {
            show_run_detail: true,
            run_detail: None,
            ..runs_app(vec![run_card(1, "PROJ-1", "backend", RunStatus::Running)])
        };
        let text = buffer_text(&render_with_size(&app, 112, 24));
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
            agent_usage: vec![],
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

    fn run_detail_with_last_event(
        status: RunStatus,
        last_event_kind: &str,
    ) -> crate::tui::app::RunDetail {
        crate::tui::app::RunDetail {
            id: 1,
            ticket: "PROJ-1".to_string(),
            lane: "backend".to_string(),
            kind: "lane".to_string(),
            status,
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
            events: vec![crate::tui::app::RunDetailEvent {
                at: "2020-01-01T00:00:01.000Z".to_string(),
                kind: last_event_kind.to_string(),
                detail: None,
            }],
            checklist: None,
            tool_counts: vec![],
            model_usage: None,
            agent_usage: vec![],
        }
    }

    #[test]
    fn run_detail_overlay_shows_waiting_next_to_status_when_awaiting_input() {
        let app = App {
            show_run_detail: true,
            run_detail: Some(run_detail_with_last_event(RunStatus::Running, "await")),
            ..runs_app(vec![run_card(1, "PROJ-1", "backend", RunStatus::Running)])
        };
        // The identity column is roughly a third of the window's width; a
        // narrow terminal clips "status: running (waiting)" before the
        // marker, same as non_lane_run_card_renders_its_kind_badge_styled's
        // rationale for widening its render.
        let buffer = render_with_size(&app, 160, 24);
        let text = buffer_text(&buffer);
        assert!(text.contains("(waiting)"));
        let cell = cell_at(&buffer, "(waiting)").expect("waiting marker renders");
        assert_eq!(Some(cell.fg), theme::AWAITING_INPUT.fg);
    }

    #[test]
    fn run_detail_overlay_omits_waiting_when_last_event_is_not_await() {
        let app = App {
            show_run_detail: true,
            run_detail: Some(run_detail_with_last_event(RunStatus::Running, "tool")),
            ..runs_app(vec![run_card(1, "PROJ-1", "backend", RunStatus::Running)])
        };
        let text = buffer_text(&render(&app));
        assert!(!text.contains("(waiting)"));
    }

    #[test]
    fn run_detail_overlay_omits_waiting_for_a_finished_run_with_trailing_await() {
        let app = App {
            show_run_detail: true,
            run_detail: Some(run_detail_with_last_event(RunStatus::Done, "await")),
            ..runs_app(vec![run_card(1, "PROJ-1", "backend", RunStatus::Running)])
        };
        let text = buffer_text(&render(&app));
        assert!(!text.contains("(waiting)"));
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
        assert!(text.contains("Checklist 2/3"));
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
    fn run_detail_overlay_with_no_checklist_shows_a_placeholder() {
        // Rewritten for the panel-grid redesign: the Checklist panel always
        // renders (a stable frame across refreshes), so an absent checklist
        // now means a placeholder, not a missing section.
        let app = App {
            show_run_detail: true,
            run_detail: Some(run_detail_fixture()),
            ..runs_app(vec![run_card(1, "PROJ-1", "backend", RunStatus::Running)])
        };
        let buffer = render(&app);
        let text = buffer_text(&buffer);
        assert!(text.contains("Checklist"));
        assert!(text.contains("no checklist"));
        let cell = cell_at(&buffer, "no checklist").expect("placeholder renders");
        assert_eq!(Some(cell.fg), theme::DIM.fg);
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
    fn run_detail_overlay_renders_agent_usage_section() {
        let detail = crate::tui::app::RunDetail {
            agent_usage: vec!["elixir-implementer  3x, out 1.1k, in 2, cache-read 87.5k, cache-write 3.0k, tools 38".to_string()],
            ..run_detail_fixture()
        };
        let app = App {
            show_run_detail: true,
            run_detail: Some(detail),
            ..runs_app(vec![run_card(1, "PROJ-1", "backend", RunStatus::Running)])
        };
        let text = buffer_text(&render(&app));
        assert!(text.contains("Agent usage"));
        assert!(text.contains("elixir-implementer"));
    }

    #[test]
    fn run_detail_overlay_with_no_agent_usage_has_no_agent_usage_section() {
        let app = App {
            show_run_detail: true,
            run_detail: Some(run_detail_fixture()),
            ..runs_app(vec![run_card(1, "PROJ-1", "backend", RunStatus::Running)])
        };
        let text = buffer_text(&render(&app));
        assert!(!text.contains("Agent usage"));
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
            agent_usage: vec![],
        }
    }

    #[test]
    fn run_detail_overlay_with_no_usage_shows_a_placeholder() {
        let app = App {
            show_run_detail: true,
            run_detail: Some(run_detail_fixture()),
            ..runs_app(vec![run_card(1, "PROJ-1", "backend", RunStatus::Running)])
        };
        let buffer = render(&app);
        let text = buffer_text(&buffer);
        assert!(text.contains("Usage"));
        assert!(text.contains("no usage yet"));
        let cell = cell_at(&buffer, "no usage yet").expect("placeholder renders");
        assert_eq!(Some(cell.fg), theme::DIM.fg);
    }

    #[test]
    fn run_detail_overlay_truncates_an_overflowing_checklist_with_a_more_marker() {
        let items: Vec<(&str, bool)> = (0..20).map(|_| ("task", false)).collect();
        let detail = crate::tui::app::RunDetail {
            checklist: Some(checklist(&items)),
            ..run_detail_fixture()
        };
        let app = App {
            show_run_detail: true,
            run_detail: Some(detail),
            ..runs_app(vec![run_card(1, "PROJ-1", "backend", RunStatus::Running)])
        };
        let text = buffer_text(&render_with_size(&app, 100, 40));
        assert!(
            text.contains("more"),
            "20 checklist items must overflow the fixed-height panel: {text}"
        );
        assert!(
            !text.contains("\u{2026}"),
            "house rule is ASCII dots, not an ellipsis"
        );
        assert!(text.contains("..."));
    }

    #[test]
    fn run_detail_overlay_truncates_overflowing_agent_usage_with_a_more_marker() {
        let agent_usage: Vec<String> = (0..20).map(|n| format!("agent-{n} usage")).collect();
        let detail = crate::tui::app::RunDetail {
            agent_usage,
            ..run_detail_fixture()
        };
        let app = App {
            show_run_detail: true,
            run_detail: Some(detail),
            ..runs_app(vec![run_card(1, "PROJ-1", "backend", RunStatus::Running)])
        };
        let text = buffer_text(&render_with_size(&app, 100, 40));
        assert!(
            text.contains("... +"),
            "20 agent usage lines must overflow the fixed-height panel: {text}"
        );
    }

    #[test]
    fn run_detail_overlay_scroll_moves_events_but_leaves_header_intact() {
        let events: Vec<crate::tui::app::RunDetailEvent> = (0..8)
            .map(|n| crate::tui::app::RunDetailEvent {
                at: format!("2020-01-01T00:00:{n:02}.000Z"),
                kind: format!("event-{n}"),
                detail: None,
            })
            .collect();
        let detail = crate::tui::app::RunDetail {
            events,
            ..run_detail_fixture()
        };
        let app_at = |scroll: u16| App {
            show_run_detail: true,
            run_detail: Some(detail.clone()),
            run_detail_scroll: scroll,
            ..runs_app(vec![run_card(1, "PROJ-1", "backend", RunStatus::Running)])
        };

        let unscrolled = render_with_size(&app_at(0), 100, 40);
        let scrolled = render_with_size(&app_at(1), 100, 40);

        // The header's "lane: backend" line does not move when only the
        // events panel scrolls.
        let lane_before = cell_pos(&unscrolled, "backend").expect("lane value renders");
        let lane_after = cell_pos(&scrolled, "backend").expect("lane value renders after scroll");
        assert_eq!(lane_before, lane_after);

        // Newest-first events: unscrolled shows "event-7" as the topmost
        // event line; scrolling by one row drops it off the top.
        assert!(buffer_text(&unscrolled).contains("event-7"));
        assert!(!buffer_text(&scrolled).contains("event-7"));
    }

    #[test]
    fn run_detail_overlay_renders_on_board_when_show_run_detail() {
        let app = App {
            screen: Screen::Board,
            show_run_detail: true,
            run_detail: Some(run_detail_fixture()),
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        assert!(text.contains("Run 1: PROJ-1"));
    }

    #[test]
    fn run_detail_overlay_does_not_render_on_board_when_show_run_detail_is_false() {
        let app = App {
            screen: Screen::Board,
            show_run_detail: false,
            run_detail: Some(run_detail_fixture()),
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        assert!(!text.contains("Run 1: PROJ-1"));
    }

    #[test]
    fn run_detail_overlay_does_not_render_on_rank_even_when_show_run_detail() {
        let app = App {
            screen: Screen::Rank,
            show_run_detail: true,
            run_detail: Some(run_detail_fixture()),
            ..App::new()
        };
        let text = buffer_text(&render(&app));
        assert!(!text.contains("Run 1: PROJ-1"));
    }

    #[test]
    fn board_hint_switches_to_the_overlay_variant_while_open() {
        let closed = hint_for(Screen::Board, false);
        let open = hint_for(Screen::Board, true);
        assert!(closed.contains("v view run"));
        assert!(open.contains("Esc/q close"));
        assert!(open.contains("j/k scroll"));
        assert!(open.contains("r refresh"));
        assert_ne!(closed, open);
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
        let text = buffer_text(&render_with_size(&runs_app(vec![card]), 112, 24));
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
