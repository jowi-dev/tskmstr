//! Named style constants for the TUI.
//!
//! Everything here is a `const` [`Style`] (or a `const fn` mapping a value to
//! one), so the rest of `tui` never writes an inline `Style::default()...`
//! for anything covered here. The palette is deliberately restrained:
//! colored accents laid over an otherwise default-colored UI, never a
//! background color, so the TUI still respects the user's terminal theme
//! (light or dark) instead of fighting it.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

use crate::runs::RunStatus;

/// Bold, default color. Used for board/runs column titles and every other
/// floating window's border title.
pub const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);

/// A board/runs column's title: bold, default color. Alias of [`BOLD`].
pub const COLUMN_TITLE: Style = BOLD;

/// The selected board column's border: bold yellow. Predates this module;
/// kept exactly as it was so its cell-level test contract is unchanged.
pub const SELECTED_COLUMN_BORDER: Style =
    Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);

/// A ticket or run card's key/id: bold, default color.
pub const CARD_KEY: Style = Style::new().add_modifier(Modifier::BOLD);

/// Secondary, low-emphasis text: timestamps, ages, hints. Dark gray so it
/// recedes without disappearing against either a light or dark terminal
/// background.
pub const DIM: Style = Style::new().fg(Color::DarkGray);

/// A completed checklist item (`[x]`): green.
pub const CHECKLIST_DONE: Style = Style::new().fg(Color::Green);

/// A pending checklist item (`[ ]`): dim, matching [`DIM`].
pub const CHECKLIST_PENDING: Style = DIM;

/// A detail window's section header (`Checklist`, `Model usage`, `Agent
/// usage`, `Tools`): bold cyan.
pub const SECTION_HEADER: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);

/// The stale-heartbeat marker (`!`) on a running run card whose heartbeat has
/// gone quiet: bold red. Predates this module; kept exactly as it was.
pub const STALE_MARKER: Style = Style::new().fg(Color::Red).add_modifier(Modifier::BOLD);

/// The active filter's leading `*` marker in the assignee filter picker:
/// yellow.
pub const ACTIVE_FILTER_MARKER: Style = Style::new().fg(Color::Yellow);

/// The rank screen's grabbed-row marker (`><`): bold yellow.
pub const GRABBED_MARKER: Style = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);

/// The style for a run's status: the color used for its kanban column title
/// and its cards' status accent.
///
/// Queued is dim gray (waiting, nothing happening yet), running is cyan
/// (active), blocked is red (needs attention), review is magenta (needs a
/// human), done is green (success), failed is red (needs attention).
pub const fn run_status_style(status: RunStatus) -> Style {
    let color = match status {
        RunStatus::Queued => Color::DarkGray,
        RunStatus::Running => Color::Cyan,
        RunStatus::Blocked => Color::Red,
        RunStatus::Review => Color::Magenta,
        RunStatus::Done => Color::Green,
        RunStatus::Failed => Color::Red,
    };
    Style::new().fg(color)
}

/// The style for a run's `kind` badge (e.g. `audit`, `create`, `lane`).
///
/// `audit` is yellow, `create` is blue, and anything else (including the
/// default `lane`) gets the terminal's default color: `lane` runs are the
/// common case and shouldn't compete visually with the two session kinds
/// this badge exists to call out.
pub fn kind_style(kind: &str) -> Style {
    match kind {
        "audit" => Style::new().fg(Color::Yellow),
        "create" => Style::new().fg(Color::Blue),
        _ => Style::new(),
    }
}

/// A run's `kind` rendered as a badge span (` <kind>`, leading space so it
/// reads naturally appended to another line), styled with [`kind_style`].
/// Returns `None` for the default `lane` kind, which gets no badge.
pub fn kind_badge(kind: &str) -> Option<Span<'static>> {
    if kind == "lane" {
        return None;
    }
    Some(Span::styled(format!(" {kind}"), kind_style(kind)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_status_style_maps_every_status_to_its_color() {
        assert_eq!(
            run_status_style(RunStatus::Queued).fg,
            Some(Color::DarkGray)
        );
        assert_eq!(run_status_style(RunStatus::Running).fg, Some(Color::Cyan));
        assert_eq!(run_status_style(RunStatus::Blocked).fg, Some(Color::Red));
        assert_eq!(run_status_style(RunStatus::Review).fg, Some(Color::Magenta));
        assert_eq!(run_status_style(RunStatus::Done).fg, Some(Color::Green));
        assert_eq!(run_status_style(RunStatus::Failed).fg, Some(Color::Red));
    }

    #[test]
    fn kind_style_colors_audit_and_create_distinctly() {
        assert_eq!(kind_style("audit").fg, Some(Color::Yellow));
        assert_eq!(kind_style("create").fg, Some(Color::Blue));
        assert_eq!(kind_style("lane").fg, None);
        assert_eq!(kind_style("something-else").fg, None);
    }

    #[test]
    fn kind_badge_is_none_for_lane() {
        assert!(kind_badge("lane").is_none());
    }

    #[test]
    fn kind_badge_renders_leading_space_and_kind_style() {
        let badge = kind_badge("audit").expect("audit gets a badge");
        assert_eq!(badge.content.as_ref(), " audit");
        assert_eq!(badge.style.fg, Some(Color::Yellow));
    }
}
