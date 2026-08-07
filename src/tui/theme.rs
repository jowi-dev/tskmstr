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
use crate::tui::app::{AuditIndicator, BotWatchIndicator, RunIndicator};

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

/// The waiting-for-input marker on a running run card whose most recent
/// event is `await` (see [`crate::runs::is_awaiting_input`]): bold yellow,
/// the loud one — an idling session that looks identical to a hung one
/// otherwise.
pub const AWAITING_INPUT: Style = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);

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

/// The style for a Jira ticket's status *category* (the stable `new` /
/// `indeterminate` / `done` key, not the free-text status name): board
/// column titles and the detail overlay's status value share this.
///
/// `new` is blue (not started), `indeterminate` is cyan (in progress),
/// `done` is green (finished); any other category (Jira allows custom
/// ones) gets the terminal's default color rather than guessing.
pub fn ticket_status_style(status_category: &str) -> Style {
    match status_category {
        "new" => Style::new().fg(Color::Blue),
        "indeterminate" => Style::new().fg(Color::Cyan),
        "done" => Style::new().fg(Color::Green),
        _ => Style::new(),
    }
}

/// The style for a board ticket's [`AuditIndicator`] badge: `Waiting` is the
/// loud one (bold yellow, matching [`AWAITING_INPUT`] -- an idling audit
/// session looks identical to a hung one otherwise), `Running` cyan
/// (active), `Starting` dim (not live yet), `Done` green (success), `Failed`
/// red (needs attention).
pub fn audit_indicator_style(indicator: AuditIndicator) -> Style {
    match indicator {
        AuditIndicator::Waiting => AWAITING_INPUT,
        AuditIndicator::Running => Style::new().fg(Color::Cyan),
        AuditIndicator::Starting => DIM,
        AuditIndicator::Done => Style::new().fg(Color::Green),
        AuditIndicator::Failed => Style::new().fg(Color::Red),
    }
}

/// Short label text for `indicator`, rendered as a board ticket card's audit
/// badge line (see [`audit_indicator_style`] for its color).
pub fn audit_indicator_label(indicator: AuditIndicator) -> &'static str {
    match indicator {
        AuditIndicator::Starting => "audit: starting",
        AuditIndicator::Running => "audit: running",
        AuditIndicator::Waiting => "audit: waiting",
        AuditIndicator::Done => "audit: done",
        AuditIndicator::Failed => "audit: failed",
    }
}

/// The style for a board ticket's [`RunIndicator`] badge: identical per-state
/// colors to [`audit_indicator_style`] (`Waiting` bold yellow, `Running`
/// cyan, `Starting` dim, `Done` green, `Failed` red), since both badges
/// signal the same underlying run lifecycle -- just for different `kind`s of
/// run.
pub fn run_indicator_style(indicator: RunIndicator) -> Style {
    match indicator {
        RunIndicator::Waiting => AWAITING_INPUT,
        RunIndicator::Running => Style::new().fg(Color::Cyan),
        RunIndicator::Starting => DIM,
        RunIndicator::Done => Style::new().fg(Color::Green),
        RunIndicator::Failed => Style::new().fg(Color::Red),
    }
}

/// Short label text for `indicator`, rendered as a board ticket card's
/// lane-run badge line (see [`run_indicator_style`] for its color).
pub fn run_indicator_label(indicator: RunIndicator) -> &'static str {
    match indicator {
        RunIndicator::Starting => "run: starting",
        RunIndicator::Running => "run: running",
        RunIndicator::Waiting => "run: waiting",
        RunIndicator::Done => "run: done",
        RunIndicator::Failed => "run: failed",
    }
}

/// The style for a board ticket's [`BotWatchIndicator`] badge: `Ready` is the
/// loud one (bold yellow, matching [`AWAITING_INPUT`] -- it is the state that
/// wants a keypress, exactly like an audit session waiting on input),
/// `Watching` cyan (active), `Clean` green (nothing to do), `Failed` red
/// (needs attention).
pub fn bot_watch_indicator_style(indicator: BotWatchIndicator) -> Style {
    match indicator {
        BotWatchIndicator::Ready => AWAITING_INPUT,
        BotWatchIndicator::Watching => Style::new().fg(Color::Cyan),
        BotWatchIndicator::Clean => Style::new().fg(Color::Green),
        BotWatchIndicator::Failed => Style::new().fg(Color::Red),
    }
}

/// Short label text for `indicator`, rendered as a board ticket card's
/// bot-watch badge line (see [`bot_watch_indicator_style`] for its color).
pub fn bot_watch_indicator_label(indicator: BotWatchIndicator) -> &'static str {
    match indicator {
        BotWatchIndicator::Watching => "bots: watching",
        BotWatchIndicator::Ready => "bots: ready",
        BotWatchIndicator::Clean => "bots: clean",
        BotWatchIndicator::Failed => "bots: failed",
    }
}

/// The `bots:` badge shown while a `tm pr watch` launcher child is still in
/// flight, before any `review-watch` run row exists (see
/// [`crate::tui::app::App::pending_bot_watch_launches`]). Styled [`DIM`], the
/// same not-live-yet treatment [`crate::tui::app::RunIndicator::Starting`]
/// gets. Its own constant rather than a [`BotWatchIndicator`] variant because
/// no watcher run status ever maps to it -- it exists only ahead of the run
/// row.
pub const BOT_WATCH_STARTING_LABEL: &str = "bots: starting";

/// The style for a board ticket's bugbot-cleanup badge. Takes an
/// [`AuditIndicator`] because a cleanup session *is* an audit-shaped
/// tmux-hosted session (see `docs/plans/bugbot-watch.md`'s "Board
/// integration"), but gets its own accent for the active state -- magenta
/// rather than [`audit_indicator_style`]'s cyan -- so the two badges stay
/// tellable apart on a card carrying both. `Waiting` is loud
/// ([`AWAITING_INPUT`]) for the same reason it is on an audit.
pub fn cleanup_indicator_style(indicator: AuditIndicator) -> Style {
    match indicator {
        AuditIndicator::Waiting => AWAITING_INPUT,
        AuditIndicator::Running => Style::new().fg(Color::Magenta),
        AuditIndicator::Starting => DIM,
        AuditIndicator::Done => Style::new().fg(Color::Green),
        AuditIndicator::Failed => Style::new().fg(Color::Red),
    }
}

/// Short label text for `indicator`, rendered as a board ticket card's
/// bugbot-cleanup badge line (see [`cleanup_indicator_style`] for its color).
pub fn cleanup_indicator_label(indicator: AuditIndicator) -> &'static str {
    match indicator {
        AuditIndicator::Starting => "clean: starting",
        AuditIndicator::Running => "clean: running",
        AuditIndicator::Waiting => "clean: waiting",
        AuditIndicator::Done => "clean: done",
        AuditIndicator::Failed => "clean: failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticket_status_style_maps_every_known_category_to_its_color() {
        assert_eq!(ticket_status_style("new").fg, Some(Color::Blue));
        assert_eq!(ticket_status_style("indeterminate").fg, Some(Color::Cyan));
        assert_eq!(ticket_status_style("done").fg, Some(Color::Green));
        assert_eq!(ticket_status_style("something-else").fg, None);
    }

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

    #[test]
    fn awaiting_input_is_bold_yellow() {
        assert_eq!(AWAITING_INPUT.fg, Some(Color::Yellow));
        assert!(AWAITING_INPUT.add_modifier.contains(Modifier::BOLD));
        // fg-accent-only doctrine: no background color anywhere in the
        // theme, including this new marker.
        assert_eq!(AWAITING_INPUT.bg, None);
    }

    #[test]
    fn audit_indicator_style_maps_every_indicator_to_its_color() {
        let waiting = audit_indicator_style(AuditIndicator::Waiting);
        assert_eq!(waiting.fg, Some(Color::Yellow));
        assert!(waiting.add_modifier.contains(Modifier::BOLD));
        assert_eq!(
            audit_indicator_style(AuditIndicator::Running).fg,
            Some(Color::Cyan)
        );
        assert_eq!(
            audit_indicator_style(AuditIndicator::Starting).fg,
            Some(Color::DarkGray)
        );
        assert_eq!(
            audit_indicator_style(AuditIndicator::Done).fg,
            Some(Color::Green)
        );
        assert_eq!(
            audit_indicator_style(AuditIndicator::Failed).fg,
            Some(Color::Red)
        );
    }

    #[test]
    fn audit_indicator_style_never_sets_a_background() {
        for indicator in [
            AuditIndicator::Starting,
            AuditIndicator::Running,
            AuditIndicator::Waiting,
            AuditIndicator::Done,
            AuditIndicator::Failed,
        ] {
            assert_eq!(audit_indicator_style(indicator).bg, None);
        }
    }

    #[test]
    fn audit_indicator_label_is_short_and_distinct() {
        let labels = [
            audit_indicator_label(AuditIndicator::Starting),
            audit_indicator_label(AuditIndicator::Running),
            audit_indicator_label(AuditIndicator::Waiting),
            audit_indicator_label(AuditIndicator::Done),
            audit_indicator_label(AuditIndicator::Failed),
        ];
        let unique: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(unique.len(), labels.len(), "labels must be distinct");
        for label in labels {
            assert!(label.starts_with("audit: "));
        }
    }

    #[test]
    fn run_indicator_style_maps_every_indicator_to_its_color() {
        let waiting = run_indicator_style(RunIndicator::Waiting);
        assert_eq!(waiting.fg, Some(Color::Yellow));
        assert!(waiting.add_modifier.contains(Modifier::BOLD));
        assert_eq!(
            run_indicator_style(RunIndicator::Running).fg,
            Some(Color::Cyan)
        );
        assert_eq!(
            run_indicator_style(RunIndicator::Starting).fg,
            Some(Color::DarkGray)
        );
        assert_eq!(
            run_indicator_style(RunIndicator::Done).fg,
            Some(Color::Green)
        );
        assert_eq!(
            run_indicator_style(RunIndicator::Failed).fg,
            Some(Color::Red)
        );
    }

    #[test]
    fn run_indicator_style_never_sets_a_background() {
        for indicator in [
            RunIndicator::Starting,
            RunIndicator::Running,
            RunIndicator::Waiting,
            RunIndicator::Done,
            RunIndicator::Failed,
        ] {
            assert_eq!(run_indicator_style(indicator).bg, None);
        }
    }

    #[test]
    fn bot_watch_indicator_style_maps_every_indicator_to_a_distinct_color() {
        let ready = bot_watch_indicator_style(BotWatchIndicator::Ready);
        assert_eq!(ready.fg, Some(Color::Yellow));
        assert!(
            ready.add_modifier.contains(Modifier::BOLD),
            "Ready is the act-on-me state and must be loud"
        );
        assert_eq!(
            bot_watch_indicator_style(BotWatchIndicator::Watching).fg,
            Some(Color::Cyan)
        );
        assert_eq!(
            bot_watch_indicator_style(BotWatchIndicator::Clean).fg,
            Some(Color::Green)
        );
        assert_eq!(
            bot_watch_indicator_style(BotWatchIndicator::Failed).fg,
            Some(Color::Red)
        );
        let colors: std::collections::HashSet<_> = [
            BotWatchIndicator::Watching,
            BotWatchIndicator::Ready,
            BotWatchIndicator::Clean,
            BotWatchIndicator::Failed,
        ]
        .iter()
        .map(|i| bot_watch_indicator_style(*i).fg)
        .collect();
        assert_eq!(colors.len(), 4, "each variant needs its own fg");
    }

    #[test]
    fn bot_watch_indicator_style_never_sets_a_background() {
        for indicator in [
            BotWatchIndicator::Watching,
            BotWatchIndicator::Ready,
            BotWatchIndicator::Clean,
            BotWatchIndicator::Failed,
        ] {
            assert_eq!(bot_watch_indicator_style(indicator).bg, None);
        }
    }

    #[test]
    fn bot_watch_indicator_label_is_short_and_distinct() {
        let labels = [
            bot_watch_indicator_label(BotWatchIndicator::Watching),
            bot_watch_indicator_label(BotWatchIndicator::Ready),
            bot_watch_indicator_label(BotWatchIndicator::Clean),
            bot_watch_indicator_label(BotWatchIndicator::Failed),
        ];
        let unique: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(unique.len(), labels.len(), "labels must be distinct");
        for label in labels {
            assert!(label.starts_with("bots: "));
        }
        assert!(BOT_WATCH_STARTING_LABEL.starts_with("bots: "));
    }

    #[test]
    fn cleanup_indicator_style_maps_every_indicator_to_a_distinct_color() {
        let waiting = cleanup_indicator_style(AuditIndicator::Waiting);
        assert_eq!(waiting.fg, Some(Color::Yellow));
        assert!(waiting.add_modifier.contains(Modifier::BOLD));
        assert_eq!(
            cleanup_indicator_style(AuditIndicator::Running).fg,
            Some(Color::Magenta),
            "a cleanup session gets its own accent, not audit's cyan"
        );
        assert_eq!(
            cleanup_indicator_style(AuditIndicator::Starting).fg,
            Some(Color::DarkGray)
        );
        assert_eq!(
            cleanup_indicator_style(AuditIndicator::Done).fg,
            Some(Color::Green)
        );
        assert_eq!(
            cleanup_indicator_style(AuditIndicator::Failed).fg,
            Some(Color::Red)
        );
        let colors: std::collections::HashSet<_> = [
            AuditIndicator::Starting,
            AuditIndicator::Running,
            AuditIndicator::Waiting,
            AuditIndicator::Done,
            AuditIndicator::Failed,
        ]
        .iter()
        .map(|i| cleanup_indicator_style(*i).fg)
        .collect();
        assert_eq!(colors.len(), 5, "each variant needs its own fg");
    }

    #[test]
    fn cleanup_indicator_style_never_sets_a_background() {
        for indicator in [
            AuditIndicator::Starting,
            AuditIndicator::Running,
            AuditIndicator::Waiting,
            AuditIndicator::Done,
            AuditIndicator::Failed,
        ] {
            assert_eq!(cleanup_indicator_style(indicator).bg, None);
        }
    }

    #[test]
    fn cleanup_indicator_label_is_short_and_distinct() {
        let labels = [
            cleanup_indicator_label(AuditIndicator::Starting),
            cleanup_indicator_label(AuditIndicator::Running),
            cleanup_indicator_label(AuditIndicator::Waiting),
            cleanup_indicator_label(AuditIndicator::Done),
            cleanup_indicator_label(AuditIndicator::Failed),
        ];
        let unique: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(unique.len(), labels.len(), "labels must be distinct");
        for label in labels {
            assert!(label.starts_with("clean: "));
        }
    }

    #[test]
    fn run_indicator_label_is_short_and_distinct() {
        let labels = [
            run_indicator_label(RunIndicator::Starting),
            run_indicator_label(RunIndicator::Running),
            run_indicator_label(RunIndicator::Waiting),
            run_indicator_label(RunIndicator::Done),
            run_indicator_label(RunIndicator::Failed),
        ];
        let unique: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(unique.len(), labels.len(), "labels must be distinct");
        for label in labels {
            assert!(label.starts_with("run: "));
        }
    }
}
