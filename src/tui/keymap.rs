//! Keybindings: maps a raw key press to a [`Msg`], independent of terminal
//! I/O so the bindings can be tested without a real terminal.

use crossterm::event::KeyCode;

use crate::tui::app::{Msg, Screen};

/// Which of [`Screen::Retro`]'s two floating overlays (if any) is currently
/// shown, bundled into one parameter rather than two separate booleans
/// (mirroring the mutual exclusivity of the app's `show_retro_severity_picker`
/// and `show_retro_note_entry` fields -- only one is ever set at a time).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RetroOverlay {
    /// Neither overlay is shown; ordinary [`Screen::Retro`] bindings apply.
    #[default]
    None,
    /// The defect-severity picker is shown.
    SeverityPicker,
    /// The note-entry overlay is shown.
    NoteEntry,
}

/// Key bindings active on [`Screen::Rank`]. `Enter`/`Space` grab or drop the
/// highlighted ticket; every other binding falls through to the shared
/// bindings below (navigation, `r`, `o`, `?`, `Esc`/`q`).
fn map_rank_key(key: KeyCode) -> Option<Msg> {
    match key {
        KeyCode::Enter | KeyCode::Char(' ') => Some(Msg::RankGrabToggle),
        _ => None,
    }
}

/// Whether `key` must be inert on [`Screen::Rank`] while a ticket is
/// grabbed, rather than falling through to its usual shared binding.
/// Currently just `r`: refreshing mid-grab would replace `rank_tickets` out
/// from under the pending, undropped reorder.
fn is_inert_while_rank_grabbed(key: KeyCode) -> bool {
    matches!(key, KeyCode::Char('r'))
}

/// Map a key press to the [`Msg`] it produces, or `None` if the key is
/// unbound.
///
/// The same bindings apply on every screen; `update` interprets each `Msg`
/// according to `screen` (e.g. `Up`/`Down` scroll on the detail screen but
/// move the selection on the board). `screen` is accepted here so the
/// signature stays stable if a future binding needs to vary by screen, but no
/// current binding depends on it.
///
/// While the help overlay is shown (`show_help`), every key closes it except
/// `q`, which quits the application outright.
///
/// While the assignee filter picker is shown (`show_filter_picker`), only
/// `j`/`k`/arrows (navigate), `Enter` (select), and `Esc`/`q` (close without
/// changing the filter) are bound; every other key is inert, same as board
/// keys are inert while the help overlay is up.
///
/// While the board assign picker is shown (`show_assign_picker`), the same
/// `j`/`k`/arrows/`Enter`/`Esc`/`q` shape applies, routed to `AssignPicker*`
/// instead of `FilterPicker*`.
///
/// While the lane picker is shown (`show_lane_picker`), the same
/// `j`/`k`/arrows/`Enter`/`Esc`/`q` shape applies, routed to `LanePicker*`
/// instead of `FilterPicker*`.
///
/// While the browser picker is shown (`show_browser_picker`), the same
/// `j`/`k`/arrows/`Enter`/`Esc`/`q` shape applies again, routed to
/// `BrowserPicker*` (see [`Msg::OpenBrowserAction`]).
///
/// While a ticket is grabbed on [`Screen::Rank`] (`rank_grabbed`), `r` is
/// inert: refreshing would silently discard the pending, undropped reorder.
/// `rank_grabbed` is ignored on every other screen.
///
/// While the run detail overlay is shown on [`Screen::Runs`] or
/// [`Screen::Board`] (`show_run_detail`), only `j`/`k`/arrows (scroll),
/// `Esc`/`q` (close the overlay), and `r` (refresh) are bound; every other
/// key is inert. `show_run_detail` is ignored on every other screen.
///
/// `o` and `O` (shift-`o`) both open a browser, but differ in what: on
/// [`Screen::Board`], lowercase `o` maps to [`Msg::OpenBrowserAction`], which
/// resolves whether the selected ticket has an open GitHub PR before deciding
/// whether to show the browser picker or open Jira directly (see that
/// message's doc comment). On every other screen -- and for uppercase `O` on
/// *every* screen, including [`Screen::Board`] -- both map to the original
/// [`Msg::OpenInBrowser`], which always opens Jira immediately with no PR
/// lookup. `O` is the escape hatch that preserves that always-instant
/// behavior even where `o` no longer does.
///
/// While [`Screen::Retro`]'s severity picker is shown
/// (`retro_overlay == RetroOverlay::SeverityPicker`), the same
/// `j`/`k`/arrows/`Enter`/`Esc`/`q` shape applies again, routed to
/// `RetroSeverityPicker*`. While its note-entry overlay is shown
/// (`retro_overlay == RetroOverlay::NoteEntry`), every printable character
/// is bound to `Msg::RetroNoteChar` (so typing `q` or `?` inserts a
/// character rather than quitting or opening help), `Backspace` deletes the
/// last one, `Enter` submits, and `Esc` cancels -- deliberately not `q`,
/// since `q` needs to be typeable.
///
/// [`Msg::OpenBrowserAction`]: crate::tui::app::Msg::OpenBrowserAction
/// [`Msg::OpenInBrowser`]: crate::tui::app::Msg::OpenInBrowser
#[allow(clippy::too_many_arguments)]
pub fn map_key(
    screen: &Screen,
    show_help: bool,
    show_filter_picker: bool,
    show_assign_picker: bool,
    show_lane_picker: bool,
    show_browser_picker: bool,
    rank_grabbed: bool,
    show_run_detail: bool,
    retro_overlay: RetroOverlay,
    key: KeyCode,
) -> Option<Msg> {
    if show_help {
        return Some(match key {
            KeyCode::Char('q') => Msg::Quit,
            _ => Msg::ToggleHelp,
        });
    }

    if show_filter_picker {
        return match key {
            KeyCode::Char('j') | KeyCode::Down => Some(Msg::FilterPickerDown),
            KeyCode::Char('k') | KeyCode::Up => Some(Msg::FilterPickerUp),
            KeyCode::Enter => Some(Msg::FilterPickerSelect),
            KeyCode::Esc | KeyCode::Char('q') => Some(Msg::FilterPickerClose),
            _ => None,
        };
    }

    if show_assign_picker {
        return match key {
            KeyCode::Char('j') | KeyCode::Down => Some(Msg::AssignPickerDown),
            KeyCode::Char('k') | KeyCode::Up => Some(Msg::AssignPickerUp),
            KeyCode::Enter => Some(Msg::AssignPickerSelect),
            KeyCode::Esc | KeyCode::Char('q') => Some(Msg::AssignPickerClose),
            _ => None,
        };
    }

    if show_lane_picker {
        return match key {
            KeyCode::Char('j') | KeyCode::Down => Some(Msg::LanePickerDown),
            KeyCode::Char('k') | KeyCode::Up => Some(Msg::LanePickerUp),
            KeyCode::Enter => Some(Msg::LanePickerSelect),
            KeyCode::Esc | KeyCode::Char('q') => Some(Msg::LanePickerClose),
            _ => None,
        };
    }

    if show_browser_picker {
        return match key {
            KeyCode::Char('j') | KeyCode::Down => Some(Msg::BrowserPickerDown),
            KeyCode::Char('k') | KeyCode::Up => Some(Msg::BrowserPickerUp),
            KeyCode::Enter => Some(Msg::BrowserPickerSelect),
            KeyCode::Esc | KeyCode::Char('q') => Some(Msg::BrowserPickerClose),
            _ => None,
        };
    }

    if retro_overlay == RetroOverlay::SeverityPicker {
        return match key {
            KeyCode::Char('j') | KeyCode::Down => Some(Msg::RetroSeverityPickerDown),
            KeyCode::Char('k') | KeyCode::Up => Some(Msg::RetroSeverityPickerUp),
            KeyCode::Enter => Some(Msg::RetroSeverityPickerSelect),
            KeyCode::Esc | KeyCode::Char('q') => Some(Msg::RetroSeverityPickerClose),
            _ => None,
        };
    }

    if retro_overlay == RetroOverlay::NoteEntry {
        return match key {
            KeyCode::Enter => Some(Msg::RetroNoteSubmit),
            KeyCode::Esc => Some(Msg::RetroNoteCancel),
            KeyCode::Backspace => Some(Msg::RetroNoteBackspace),
            KeyCode::Char(c) => Some(Msg::RetroNoteChar(c)),
            _ => None,
        };
    }

    if matches!(screen, Screen::Runs | Screen::Board) && show_run_detail {
        return match key {
            KeyCode::Char('j') | KeyCode::Down => Some(Msg::Down),
            KeyCode::Char('k') | KeyCode::Up => Some(Msg::Up),
            KeyCode::Esc | KeyCode::Char('q') => Some(Msg::Back),
            KeyCode::Char('r') => Some(Msg::Refresh),
            _ => None,
        };
    }

    if *screen == Screen::Rank {
        if let Some(msg) = map_rank_key(key) {
            return Some(msg);
        }
        if rank_grabbed && is_inert_while_rank_grabbed(key) {
            return None;
        }
    }

    match key {
        KeyCode::Char('j') | KeyCode::Down => Some(Msg::Down),
        KeyCode::Char('k') | KeyCode::Up => Some(Msg::Up),
        KeyCode::Char('h') | KeyCode::Left => Some(Msg::Left),
        KeyCode::Char('l') | KeyCode::Right => Some(Msg::Right),
        KeyCode::Enter => Some(Msg::Enter),
        KeyCode::Esc | KeyCode::Char('q') => Some(Msg::Back),
        KeyCode::Char('r') => Some(Msg::Refresh),
        KeyCode::Char('o') if *screen == Screen::Board => Some(Msg::OpenBrowserAction),
        KeyCode::Char('o') => Some(Msg::OpenInBrowser),
        KeyCode::Char('O') => Some(Msg::OpenInBrowser),
        KeyCode::Char('?') => Some(Msg::ToggleHelp),
        KeyCode::Char('f') if *screen == Screen::Board => Some(Msg::OpenFilterPicker),
        KeyCode::Char('A') if *screen == Screen::Board => Some(Msg::OpenAssignPicker),
        KeyCode::Char('p') if *screen == Screen::Board => Some(Msg::OpenRank),
        KeyCode::Char('a') if *screen == Screen::Board => Some(Msg::AuditAction),
        KeyCode::Char('s') if *screen == Screen::Board => Some(Msg::SessionAction),
        KeyCode::Char('w') if *screen == Screen::Board => Some(Msg::LaneRunAction),
        KeyCode::Char('b') if *screen == Screen::Board => Some(Msg::BotsAction),
        KeyCode::Char('v') if *screen == Screen::Board => Some(Msg::ViewRunAction),
        KeyCode::Char('L') if *screen == Screen::Board => Some(Msg::ViewLogsAction),
        KeyCode::Char('V') if *screen == Screen::Board => Some(Msg::ViewDiffAction),
        KeyCode::Char('F') if *screen == Screen::Board => Some(Msg::ReviewFixAction),
        KeyCode::Char('c') if *screen == Screen::Board => Some(Msg::CreateAction),
        KeyCode::Char('R') if *screen == Screen::Board => Some(Msg::OpenRetro),
        KeyCode::Char('d') if *screen == Screen::Retro => Some(Msg::RetroDefectStart),
        KeyCode::Char('c') if *screen == Screen::Retro => Some(Msg::RetroMarkClean),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_keys_map_to_up_and_down() {
        let cases = [
            (KeyCode::Char('j'), Msg::Down),
            (KeyCode::Down, Msg::Down),
            (KeyCode::Char('k'), Msg::Up),
            (KeyCode::Up, Msg::Up),
        ];
        for (key, expected) in cases {
            assert_eq!(
                map_key(
                    &Screen::Board,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    RetroOverlay::None,
                    key
                ),
                Some(expected)
            );
        }
    }

    #[test]
    fn navigation_keys_map_to_left_and_right_on_every_screen() {
        let cases = [
            (KeyCode::Char('h'), Msg::Left),
            (KeyCode::Left, Msg::Left),
            (KeyCode::Char('l'), Msg::Right),
            (KeyCode::Right, Msg::Right),
        ];
        for screen in [Screen::Board, Screen::Detail, Screen::TransitionMenu] {
            for (key, ref expected) in cases.clone() {
                assert_eq!(
                    map_key(
                        &screen,
                        false,
                        false,
                        false,
                        false,
                        false,
                        false,
                        false,
                        RetroOverlay::None,
                        key
                    ),
                    Some(expected.clone())
                );
            }
        }
    }

    #[test]
    fn enter_maps_to_enter_on_every_screen() {
        for screen in [Screen::Board, Screen::Detail, Screen::TransitionMenu] {
            assert_eq!(
                map_key(
                    &screen,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    RetroOverlay::None,
                    KeyCode::Enter
                ),
                Some(Msg::Enter)
            );
        }
    }

    #[test]
    fn esc_and_q_map_to_back_on_every_screen() {
        for screen in [Screen::Board, Screen::Detail, Screen::TransitionMenu] {
            assert_eq!(
                map_key(
                    &screen,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    RetroOverlay::None,
                    KeyCode::Esc
                ),
                Some(Msg::Back)
            );
            assert_eq!(
                map_key(
                    &screen,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    RetroOverlay::None,
                    KeyCode::Char('q')
                ),
                Some(Msg::Back)
            );
        }
    }

    #[test]
    fn r_maps_to_refresh() {
        assert_eq!(
            map_key(
                &Screen::Board,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('r')
            ),
            Some(Msg::Refresh)
        );
    }

    #[test]
    fn o_on_board_maps_to_open_browser_action() {
        assert_eq!(
            map_key(
                &Screen::Board,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('o')
            ),
            Some(Msg::OpenBrowserAction)
        );
    }

    #[test]
    fn o_off_board_maps_to_open_in_browser() {
        for screen in [
            Screen::Detail,
            Screen::TransitionMenu,
            Screen::Rank,
            Screen::Runs,
        ] {
            assert_eq!(
                map_key(
                    &screen,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    RetroOverlay::None,
                    KeyCode::Char('o')
                ),
                Some(Msg::OpenInBrowser),
                "o should still open Jira directly off the board on {screen:?}"
            );
        }
    }

    #[test]
    fn capital_o_maps_to_open_in_browser_on_every_screen() {
        for screen in [
            Screen::Board,
            Screen::Detail,
            Screen::TransitionMenu,
            Screen::Rank,
            Screen::Runs,
        ] {
            assert_eq!(
                map_key(
                    &screen,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    RetroOverlay::None,
                    KeyCode::Char('O')
                ),
                Some(Msg::OpenInBrowser),
                "O should always open Jira directly on {screen:?}"
            );
        }
    }

    #[test]
    fn browser_picker_navigation_keys_map_to_up_and_down() {
        let cases = [
            (KeyCode::Char('j'), Msg::BrowserPickerDown),
            (KeyCode::Down, Msg::BrowserPickerDown),
            (KeyCode::Char('k'), Msg::BrowserPickerUp),
            (KeyCode::Up, Msg::BrowserPickerUp),
        ];
        for (key, expected) in cases {
            assert_eq!(
                map_key(
                    &Screen::Board,
                    false,
                    false,
                    false,
                    false,
                    true,
                    false,
                    false,
                    RetroOverlay::None,
                    key
                ),
                Some(expected)
            );
        }
    }

    #[test]
    fn browser_picker_enter_selects() {
        assert_eq!(
            map_key(
                &Screen::Board,
                false,
                false,
                false,
                false,
                true,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Enter
            ),
            Some(Msg::BrowserPickerSelect)
        );
    }

    #[test]
    fn browser_picker_esc_and_q_close_it() {
        assert_eq!(
            map_key(
                &Screen::Board,
                false,
                false,
                false,
                false,
                true,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Esc
            ),
            Some(Msg::BrowserPickerClose)
        );
        assert_eq!(
            map_key(
                &Screen::Board,
                false,
                false,
                false,
                false,
                true,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('q')
            ),
            Some(Msg::BrowserPickerClose)
        );
    }

    #[test]
    fn browser_picker_open_makes_other_board_keys_inert() {
        for key in [
            KeyCode::Char('r'),
            KeyCode::Char('o'),
            KeyCode::Char('O'),
            KeyCode::Char('h'),
            KeyCode::Char('l'),
            KeyCode::Char('?'),
            KeyCode::Char('f'),
            KeyCode::Char('w'),
            KeyCode::Char('z'),
        ] {
            assert_eq!(
                map_key(
                    &Screen::Board,
                    false,
                    false,
                    false,
                    false,
                    true,
                    false,
                    false,
                    RetroOverlay::None,
                    key
                ),
                None
            );
        }
    }

    #[test]
    fn question_mark_maps_to_toggle_help() {
        assert_eq!(
            map_key(
                &Screen::Board,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('?')
            ),
            Some(Msg::ToggleHelp)
        );
    }

    #[test]
    fn unbound_key_maps_to_none() {
        assert_eq!(
            map_key(
                &Screen::Board,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('z')
            ),
            None
        );
        assert_eq!(
            map_key(
                &Screen::Board,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Tab
            ),
            None
        );
    }

    #[test]
    fn help_overlay_swallows_any_key_and_closes_it() {
        assert_eq!(
            map_key(
                &Screen::Board,
                true,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('z')
            ),
            Some(Msg::ToggleHelp)
        );
        assert_eq!(
            map_key(
                &Screen::Board,
                true,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Enter
            ),
            Some(Msg::ToggleHelp)
        );
        assert_eq!(
            map_key(
                &Screen::Board,
                true,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('r')
            ),
            Some(Msg::ToggleHelp)
        );
    }

    #[test]
    fn help_overlay_q_quits_instead_of_closing() {
        assert_eq!(
            map_key(
                &Screen::Board,
                true,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('q')
            ),
            Some(Msg::Quit)
        );
    }

    #[test]
    fn f_opens_filter_picker_on_board_screen() {
        assert_eq!(
            map_key(
                &Screen::Board,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('f')
            ),
            Some(Msg::OpenFilterPicker)
        );
    }

    #[test]
    fn f_is_unbound_off_the_board_screen() {
        for screen in [Screen::Detail, Screen::TransitionMenu] {
            assert_eq!(
                map_key(
                    &screen,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    RetroOverlay::None,
                    KeyCode::Char('f')
                ),
                None
            );
        }
    }

    #[test]
    fn filter_picker_navigation_keys_map_to_up_and_down() {
        let cases = [
            (KeyCode::Char('j'), Msg::FilterPickerDown),
            (KeyCode::Down, Msg::FilterPickerDown),
            (KeyCode::Char('k'), Msg::FilterPickerUp),
            (KeyCode::Up, Msg::FilterPickerUp),
        ];
        for (key, expected) in cases {
            assert_eq!(
                map_key(
                    &Screen::Board,
                    false,
                    true,
                    false,
                    false,
                    false,
                    false,
                    false,
                    RetroOverlay::None,
                    key
                ),
                Some(expected)
            );
        }
    }

    #[test]
    fn filter_picker_enter_selects() {
        assert_eq!(
            map_key(
                &Screen::Board,
                false,
                true,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Enter
            ),
            Some(Msg::FilterPickerSelect)
        );
    }

    #[test]
    fn filter_picker_esc_and_q_close_it() {
        assert_eq!(
            map_key(
                &Screen::Board,
                false,
                true,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Esc
            ),
            Some(Msg::FilterPickerClose)
        );
        assert_eq!(
            map_key(
                &Screen::Board,
                false,
                true,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('q')
            ),
            Some(Msg::FilterPickerClose)
        );
    }

    #[test]
    fn p_opens_rank_screen_on_board() {
        assert_eq!(
            map_key(
                &Screen::Board,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('p')
            ),
            Some(Msg::OpenRank)
        );
    }

    #[test]
    fn p_is_unbound_off_the_board_screen() {
        for screen in [Screen::Detail, Screen::TransitionMenu, Screen::Rank] {
            assert_eq!(
                map_key(
                    &screen,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    RetroOverlay::None,
                    KeyCode::Char('p')
                ),
                None
            );
        }
    }

    #[test]
    fn a_triggers_audit_action_on_board() {
        assert_eq!(
            map_key(
                &Screen::Board,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('a')
            ),
            Some(Msg::AuditAction)
        );
    }

    #[test]
    fn a_is_unbound_off_the_board_screen() {
        for screen in [
            Screen::Detail,
            Screen::TransitionMenu,
            Screen::Rank,
            Screen::Runs,
        ] {
            assert_eq!(
                map_key(
                    &screen,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    RetroOverlay::None,
                    KeyCode::Char('a')
                ),
                None
            );
        }
    }

    #[test]
    fn s_triggers_session_action_on_board() {
        assert_eq!(
            map_key(
                &Screen::Board,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('s')
            ),
            Some(Msg::SessionAction)
        );
    }

    #[test]
    fn s_is_unbound_off_the_board_screen() {
        for screen in [
            Screen::Detail,
            Screen::TransitionMenu,
            Screen::Rank,
            Screen::Runs,
        ] {
            assert_eq!(
                map_key(
                    &screen,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    RetroOverlay::None,
                    KeyCode::Char('s')
                ),
                None
            );
        }
    }

    #[test]
    fn w_triggers_lane_run_action_on_board() {
        assert_eq!(
            map_key(
                &Screen::Board,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('w')
            ),
            Some(Msg::LaneRunAction)
        );
    }

    #[test]
    fn w_is_unbound_off_the_board_screen() {
        for screen in [
            Screen::Detail,
            Screen::TransitionMenu,
            Screen::Rank,
            Screen::Runs,
        ] {
            assert_eq!(
                map_key(
                    &screen,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    RetroOverlay::None,
                    KeyCode::Char('w')
                ),
                None
            );
        }
    }

    #[test]
    fn b_triggers_bots_action_on_board() {
        assert_eq!(
            map_key(
                &Screen::Board,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('b')
            ),
            Some(Msg::BotsAction)
        );
    }

    #[test]
    fn b_is_unbound_off_the_board_screen() {
        for screen in [
            Screen::Detail,
            Screen::TransitionMenu,
            Screen::Rank,
            Screen::Runs,
        ] {
            assert_eq!(
                map_key(
                    &screen,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    RetroOverlay::None,
                    KeyCode::Char('b')
                ),
                None
            );
        }
    }

    #[test]
    fn lane_picker_navigation_keys_map_to_up_and_down() {
        let cases = [
            (KeyCode::Char('j'), Msg::LanePickerDown),
            (KeyCode::Down, Msg::LanePickerDown),
            (KeyCode::Char('k'), Msg::LanePickerUp),
            (KeyCode::Up, Msg::LanePickerUp),
        ];
        for (key, expected) in cases {
            assert_eq!(
                map_key(
                    &Screen::Board,
                    false,
                    false,
                    false,
                    true,
                    false,
                    false,
                    false,
                    RetroOverlay::None,
                    key
                ),
                Some(expected)
            );
        }
    }

    #[test]
    fn lane_picker_enter_selects() {
        assert_eq!(
            map_key(
                &Screen::Board,
                false,
                false,
                false,
                true,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Enter
            ),
            Some(Msg::LanePickerSelect)
        );
    }

    #[test]
    fn lane_picker_esc_and_q_close_it() {
        assert_eq!(
            map_key(
                &Screen::Board,
                false,
                false,
                false,
                true,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Esc
            ),
            Some(Msg::LanePickerClose)
        );
        assert_eq!(
            map_key(
                &Screen::Board,
                false,
                false,
                false,
                true,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('q')
            ),
            Some(Msg::LanePickerClose)
        );
    }

    #[test]
    fn lane_picker_open_makes_other_board_keys_inert() {
        for key in [
            KeyCode::Char('r'),
            KeyCode::Char('o'),
            KeyCode::Char('h'),
            KeyCode::Char('l'),
            KeyCode::Char('?'),
            KeyCode::Char('f'),
            KeyCode::Char('w'),
            KeyCode::Char('z'),
        ] {
            assert_eq!(
                map_key(
                    &Screen::Board,
                    false,
                    false,
                    false,
                    true,
                    false,
                    false,
                    false,
                    RetroOverlay::None,
                    key
                ),
                None
            );
        }
    }

    #[test]
    fn enter_and_space_grab_toggle_on_rank_screen() {
        assert_eq!(
            map_key(
                &Screen::Rank,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Enter
            ),
            Some(Msg::RankGrabToggle)
        );
        assert_eq!(
            map_key(
                &Screen::Rank,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char(' ')
            ),
            Some(Msg::RankGrabToggle)
        );
    }

    #[test]
    fn space_is_unbound_off_the_rank_screen() {
        for screen in [Screen::Board, Screen::Detail, Screen::TransitionMenu] {
            assert_eq!(
                map_key(
                    &screen,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    RetroOverlay::None,
                    KeyCode::Char(' ')
                ),
                None
            );
        }
    }

    #[test]
    fn rank_screen_still_maps_shared_navigation_and_action_keys() {
        assert_eq!(
            map_key(
                &Screen::Rank,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('j')
            ),
            Some(Msg::Down)
        );
        assert_eq!(
            map_key(
                &Screen::Rank,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('k')
            ),
            Some(Msg::Up)
        );
        assert_eq!(
            map_key(
                &Screen::Rank,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Esc
            ),
            Some(Msg::Back)
        );
        assert_eq!(
            map_key(
                &Screen::Rank,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('q')
            ),
            Some(Msg::Back)
        );
        assert_eq!(
            map_key(
                &Screen::Rank,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('r')
            ),
            Some(Msg::Refresh)
        );
        assert_eq!(
            map_key(
                &Screen::Rank,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('o')
            ),
            Some(Msg::OpenInBrowser)
        );
        assert_eq!(
            map_key(
                &Screen::Rank,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('?')
            ),
            Some(Msg::ToggleHelp)
        );
    }

    #[test]
    fn r_is_inert_on_rank_screen_while_a_ticket_is_grabbed() {
        assert_eq!(
            map_key(
                &Screen::Rank,
                false,
                false,
                false,
                false,
                false,
                true,
                false,
                RetroOverlay::None,
                KeyCode::Char('r')
            ),
            None
        );
    }

    #[test]
    fn r_still_refreshes_on_rank_screen_when_nothing_is_grabbed() {
        assert_eq!(
            map_key(
                &Screen::Rank,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('r')
            ),
            Some(Msg::Refresh)
        );
    }

    #[test]
    fn rank_grabbed_does_not_affect_other_keys() {
        // Only `r` is gated while grabbed; everything else on the rank
        // screen behaves the same whether or not a ticket is grabbed.
        for key in [
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Esc,
            KeyCode::Char('q'),
            KeyCode::Char('o'),
            KeyCode::Char('?'),
            KeyCode::Enter,
            KeyCode::Char(' '),
        ] {
            assert_eq!(
                map_key(
                    &Screen::Rank,
                    false,
                    false,
                    false,
                    false,
                    false,
                    true,
                    false,
                    RetroOverlay::None,
                    key
                ),
                map_key(
                    &Screen::Rank,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    RetroOverlay::None,
                    key
                ),
                "key {key:?} should behave the same grabbed or not"
            );
        }
    }

    #[test]
    fn rank_grabbed_flag_is_ignored_off_the_rank_screen() {
        for screen in [Screen::Board, Screen::Detail, Screen::TransitionMenu] {
            assert_eq!(
                map_key(
                    &screen,
                    false,
                    false,
                    false,
                    false,
                    false,
                    true,
                    false,
                    RetroOverlay::None,
                    KeyCode::Char('r')
                ),
                Some(Msg::Refresh)
            );
        }
    }

    #[test]
    fn filter_picker_open_makes_other_board_keys_inert() {
        for key in [
            KeyCode::Char('r'),
            KeyCode::Char('o'),
            KeyCode::Char('h'),
            KeyCode::Char('l'),
            KeyCode::Char('?'),
            KeyCode::Char('f'),
            KeyCode::Char('z'),
        ] {
            assert_eq!(
                map_key(
                    &Screen::Board,
                    false,
                    true,
                    false,
                    false,
                    false,
                    false,
                    false,
                    RetroOverlay::None,
                    key
                ),
                None
            );
        }
    }

    #[test]
    fn shift_a_opens_assign_picker_on_board_screen() {
        assert_eq!(
            map_key(
                &Screen::Board,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('A')
            ),
            Some(Msg::OpenAssignPicker)
        );
    }

    #[test]
    fn shift_a_is_unbound_off_the_board_screen() {
        for screen in [Screen::Runs, Screen::Detail] {
            assert_eq!(
                map_key(
                    &screen,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    RetroOverlay::None,
                    KeyCode::Char('A')
                ),
                None
            );
        }
    }

    #[test]
    fn assign_picker_navigation_keys_map_to_up_and_down() {
        let cases = [
            (KeyCode::Char('j'), Msg::AssignPickerDown),
            (KeyCode::Down, Msg::AssignPickerDown),
            (KeyCode::Char('k'), Msg::AssignPickerUp),
            (KeyCode::Up, Msg::AssignPickerUp),
        ];
        for (key, expected) in cases {
            assert_eq!(
                map_key(
                    &Screen::Board,
                    false,
                    false,
                    true,
                    false,
                    false,
                    false,
                    false,
                    RetroOverlay::None,
                    key
                ),
                Some(expected)
            );
        }
    }

    #[test]
    fn assign_picker_enter_selects() {
        assert_eq!(
            map_key(
                &Screen::Board,
                false,
                false,
                true,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Enter
            ),
            Some(Msg::AssignPickerSelect)
        );
    }

    #[test]
    fn assign_picker_esc_and_q_close_it() {
        assert_eq!(
            map_key(
                &Screen::Board,
                false,
                false,
                true,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Esc
            ),
            Some(Msg::AssignPickerClose)
        );
        assert_eq!(
            map_key(
                &Screen::Board,
                false,
                false,
                true,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('q')
            ),
            Some(Msg::AssignPickerClose)
        );
    }

    #[test]
    fn assign_picker_open_makes_other_board_keys_inert() {
        for key in [
            KeyCode::Char('r'),
            KeyCode::Char('o'),
            KeyCode::Char('h'),
            KeyCode::Char('l'),
            KeyCode::Char('?'),
            KeyCode::Char('f'),
            KeyCode::Char('A'),
            KeyCode::Char('z'),
        ] {
            assert_eq!(
                map_key(
                    &Screen::Board,
                    false,
                    false,
                    true,
                    false,
                    false,
                    false,
                    false,
                    RetroOverlay::None,
                    key
                ),
                None
            );
        }
    }

    #[test]
    fn runs_screen_maps_shared_navigation_and_action_keys() {
        assert_eq!(
            map_key(
                &Screen::Runs,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('h')
            ),
            Some(Msg::Left)
        );
        assert_eq!(
            map_key(
                &Screen::Runs,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('l')
            ),
            Some(Msg::Right)
        );
        assert_eq!(
            map_key(
                &Screen::Runs,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('j')
            ),
            Some(Msg::Down)
        );
        assert_eq!(
            map_key(
                &Screen::Runs,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('k')
            ),
            Some(Msg::Up)
        );
        assert_eq!(
            map_key(
                &Screen::Runs,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Enter
            ),
            Some(Msg::Enter)
        );
        assert_eq!(
            map_key(
                &Screen::Runs,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('r')
            ),
            Some(Msg::Refresh)
        );
        assert_eq!(
            map_key(
                &Screen::Runs,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Esc
            ),
            Some(Msg::Back)
        );
        assert_eq!(
            map_key(
                &Screen::Runs,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('q')
            ),
            Some(Msg::Back)
        );
    }

    #[test]
    fn run_detail_open_restricts_to_scroll_close_and_refresh() {
        assert_eq!(
            map_key(
                &Screen::Runs,
                false,
                false,
                false,
                false,
                false,
                false,
                true,
                RetroOverlay::None,
                KeyCode::Char('j')
            ),
            Some(Msg::Down)
        );
        assert_eq!(
            map_key(
                &Screen::Runs,
                false,
                false,
                false,
                false,
                false,
                false,
                true,
                RetroOverlay::None,
                KeyCode::Down
            ),
            Some(Msg::Down)
        );
        assert_eq!(
            map_key(
                &Screen::Runs,
                false,
                false,
                false,
                false,
                false,
                false,
                true,
                RetroOverlay::None,
                KeyCode::Char('k')
            ),
            Some(Msg::Up)
        );
        assert_eq!(
            map_key(
                &Screen::Runs,
                false,
                false,
                false,
                false,
                false,
                false,
                true,
                RetroOverlay::None,
                KeyCode::Up
            ),
            Some(Msg::Up)
        );
        assert_eq!(
            map_key(
                &Screen::Runs,
                false,
                false,
                false,
                false,
                false,
                false,
                true,
                RetroOverlay::None,
                KeyCode::Esc
            ),
            Some(Msg::Back)
        );
        assert_eq!(
            map_key(
                &Screen::Runs,
                false,
                false,
                false,
                false,
                false,
                false,
                true,
                RetroOverlay::None,
                KeyCode::Char('q')
            ),
            Some(Msg::Back)
        );
        assert_eq!(
            map_key(
                &Screen::Runs,
                false,
                false,
                false,
                false,
                false,
                false,
                true,
                RetroOverlay::None,
                KeyCode::Char('r')
            ),
            Some(Msg::Refresh)
        );
    }

    #[test]
    fn run_detail_open_makes_other_keys_inert() {
        for key in [
            KeyCode::Char('h'),
            KeyCode::Char('l'),
            KeyCode::Enter,
            KeyCode::Char('o'),
            KeyCode::Char('?'),
            KeyCode::Char('z'),
        ] {
            assert_eq!(
                map_key(
                    &Screen::Runs,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    true,
                    RetroOverlay::None,
                    key
                ),
                None
            );
        }
    }

    #[test]
    fn show_run_detail_flag_is_ignored_off_runs_and_board_screens() {
        for screen in [Screen::Detail, Screen::TransitionMenu, Screen::Rank] {
            assert_eq!(
                map_key(
                    &screen,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    true,
                    RetroOverlay::None,
                    KeyCode::Char('r')
                ),
                Some(Msg::Refresh)
            );
        }
    }

    #[test]
    fn v_maps_to_view_run_action_on_board_only() {
        assert_eq!(
            map_key(
                &Screen::Board,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('v')
            ),
            Some(Msg::ViewRunAction)
        );
        for screen in [
            Screen::Detail,
            Screen::TransitionMenu,
            Screen::Rank,
            Screen::Runs,
        ] {
            assert_eq!(
                map_key(
                    &screen,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    RetroOverlay::None,
                    KeyCode::Char('v')
                ),
                None
            );
        }
    }

    #[test]
    fn capital_l_maps_to_view_logs_action_on_board_only() {
        assert_eq!(
            map_key(
                &Screen::Board,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('L')
            ),
            Some(Msg::ViewLogsAction)
        );
        for screen in [
            Screen::Detail,
            Screen::TransitionMenu,
            Screen::Rank,
            Screen::Runs,
        ] {
            assert_eq!(
                map_key(
                    &screen,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    RetroOverlay::None,
                    KeyCode::Char('L')
                ),
                None
            );
        }
    }

    #[test]
    fn show_run_detail_gating_branch_is_active_on_board() {
        let cases = [
            (KeyCode::Char('j'), Some(Msg::Down)),
            (KeyCode::Down, Some(Msg::Down)),
            (KeyCode::Char('k'), Some(Msg::Up)),
            (KeyCode::Up, Some(Msg::Up)),
            (KeyCode::Esc, Some(Msg::Back)),
            (KeyCode::Char('q'), Some(Msg::Back)),
            (KeyCode::Char('r'), Some(Msg::Refresh)),
            (KeyCode::Char('v'), None),
            (KeyCode::Char('a'), None),
        ];
        for (key, expected) in cases {
            assert_eq!(
                map_key(
                    &Screen::Board,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    true,
                    RetroOverlay::None,
                    key
                ),
                expected
            );
        }
    }

    #[test]
    fn capital_v_maps_to_view_diff_action_on_board_only() {
        assert_eq!(
            map_key(
                &Screen::Board,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('V')
            ),
            Some(Msg::ViewDiffAction)
        );
        for screen in [
            Screen::Detail,
            Screen::TransitionMenu,
            Screen::Rank,
            Screen::Runs,
        ] {
            assert_eq!(
                map_key(
                    &screen,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    RetroOverlay::None,
                    KeyCode::Char('V')
                ),
                None
            );
        }
    }

    #[test]
    fn capital_f_maps_to_review_fix_action_on_board_only() {
        assert_eq!(
            map_key(
                &Screen::Board,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('F')
            ),
            Some(Msg::ReviewFixAction)
        );
        for screen in [
            Screen::Detail,
            Screen::TransitionMenu,
            Screen::Rank,
            Screen::Runs,
        ] {
            assert_eq!(
                map_key(
                    &screen,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    RetroOverlay::None,
                    KeyCode::Char('F')
                ),
                None
            );
        }
    }

    #[test]
    fn capital_r_opens_retro_board_on_board_only() {
        assert_eq!(
            map_key(
                &Screen::Board,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('R')
            ),
            Some(Msg::OpenRetro)
        );
        for screen in [
            Screen::Detail,
            Screen::TransitionMenu,
            Screen::Rank,
            Screen::Runs,
            Screen::Retro,
        ] {
            assert_eq!(
                map_key(
                    &screen,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    RetroOverlay::None,
                    KeyCode::Char('R')
                ),
                None
            );
        }
    }

    #[test]
    fn c_triggers_create_action_on_board() {
        assert_eq!(
            map_key(
                &Screen::Board,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('c')
            ),
            Some(Msg::CreateAction)
        );
    }

    #[test]
    fn c_is_unbound_off_the_board_screen_except_retro() {
        // On Screen::Retro, `c` marks a ticket clean (see the retro test
        // below); everywhere else off the board it stays unbound.
        for screen in [
            Screen::Detail,
            Screen::TransitionMenu,
            Screen::Rank,
            Screen::Runs,
        ] {
            assert_eq!(
                map_key(
                    &screen,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    RetroOverlay::None,
                    KeyCode::Char('c')
                ),
                None
            );
        }
    }

    #[test]
    fn d_and_c_trigger_retro_actions_on_retro_screen_only() {
        assert_eq!(
            map_key(
                &Screen::Retro,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('d')
            ),
            Some(Msg::RetroDefectStart)
        );
        assert_eq!(
            map_key(
                &Screen::Retro,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('c')
            ),
            Some(Msg::RetroMarkClean)
        );
        for screen in [
            Screen::Board,
            Screen::Detail,
            Screen::TransitionMenu,
            Screen::Rank,
            Screen::Runs,
        ] {
            for key in [KeyCode::Char('d'), KeyCode::Char('c')] {
                // `c` on the board is CreateAction (issue #15), not a stray
                // retro binding -- covered by its own tests above.
                if screen == Screen::Board && key == KeyCode::Char('c') {
                    continue;
                }
                assert_eq!(
                    map_key(
                        &screen,
                        false,
                        false,
                        false,
                        false,
                        false,
                        false,
                        false,
                        RetroOverlay::None,
                        key
                    ),
                    None,
                    "{key:?} should be unbound off the retro screen"
                );
            }
        }
    }

    #[test]
    fn retro_screen_still_maps_shared_navigation_and_action_keys() {
        assert_eq!(
            map_key(
                &Screen::Retro,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('j')
            ),
            Some(Msg::Down)
        );
        assert_eq!(
            map_key(
                &Screen::Retro,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Char('r')
            ),
            Some(Msg::Refresh)
        );
        assert_eq!(
            map_key(
                &Screen::Retro,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::None,
                KeyCode::Esc
            ),
            Some(Msg::Back)
        );
    }

    #[test]
    fn retro_severity_picker_navigation_keys_map_to_up_and_down() {
        let cases = [
            (KeyCode::Char('j'), Msg::RetroSeverityPickerDown),
            (KeyCode::Down, Msg::RetroSeverityPickerDown),
            (KeyCode::Char('k'), Msg::RetroSeverityPickerUp),
            (KeyCode::Up, Msg::RetroSeverityPickerUp),
        ];
        for (key, expected) in cases {
            assert_eq!(
                map_key(
                    &Screen::Retro,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    RetroOverlay::SeverityPicker,
                    key
                ),
                Some(expected)
            );
        }
    }

    #[test]
    fn retro_severity_picker_enter_selects_and_esc_or_q_close_it() {
        assert_eq!(
            map_key(
                &Screen::Retro,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::SeverityPicker,
                KeyCode::Enter
            ),
            Some(Msg::RetroSeverityPickerSelect)
        );
        for key in [KeyCode::Esc, KeyCode::Char('q')] {
            assert_eq!(
                map_key(
                    &Screen::Retro,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    RetroOverlay::SeverityPicker,
                    key
                ),
                Some(Msg::RetroSeverityPickerClose)
            );
        }
    }

    #[test]
    fn retro_severity_picker_open_makes_other_keys_inert() {
        for key in [
            KeyCode::Char('r'),
            KeyCode::Char('o'),
            KeyCode::Char('d'),
            KeyCode::Char('c'),
            KeyCode::Char('?'),
            KeyCode::Char('z'),
        ] {
            assert_eq!(
                map_key(
                    &Screen::Retro,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    RetroOverlay::SeverityPicker,
                    key
                ),
                None
            );
        }
    }

    #[test]
    fn retro_note_entry_types_printable_characters_including_q_and_question_mark() {
        for c in ['a', 'q', '?', ' ', 'Z'] {
            assert_eq!(
                map_key(
                    &Screen::Retro,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    RetroOverlay::NoteEntry,
                    KeyCode::Char(c)
                ),
                Some(Msg::RetroNoteChar(c)),
                "{c:?} should type into the note, not trigger its usual binding"
            );
        }
    }

    #[test]
    fn retro_note_entry_backspace_enter_and_esc() {
        assert_eq!(
            map_key(
                &Screen::Retro,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::NoteEntry,
                KeyCode::Backspace
            ),
            Some(Msg::RetroNoteBackspace)
        );
        assert_eq!(
            map_key(
                &Screen::Retro,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::NoteEntry,
                KeyCode::Enter
            ),
            Some(Msg::RetroNoteSubmit)
        );
        assert_eq!(
            map_key(
                &Screen::Retro,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                RetroOverlay::NoteEntry,
                KeyCode::Esc
            ),
            Some(Msg::RetroNoteCancel)
        );
    }
}
