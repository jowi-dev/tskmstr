//! Keybindings: maps a raw key press to a [`Msg`], independent of terminal
//! I/O so the bindings can be tested without a real terminal.

use crossterm::event::KeyCode;

use crate::tui::app::{Msg, Screen};

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
/// While a ticket is grabbed on [`Screen::Rank`] (`rank_grabbed`), `r` is
/// inert: refreshing would silently discard the pending, undropped reorder.
/// `rank_grabbed` is ignored on every other screen.
///
/// While the run detail overlay is shown on [`Screen::Runs`]
/// (`show_run_detail`), only `j`/`k`/arrows (scroll), `Esc`/`q` (close the
/// overlay), and `r` (refresh) are bound; every other key is inert.
/// `show_run_detail` is ignored on every other screen.
pub fn map_key(
    screen: &Screen,
    show_help: bool,
    show_filter_picker: bool,
    rank_grabbed: bool,
    show_run_detail: bool,
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

    if *screen == Screen::Runs && show_run_detail {
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
        KeyCode::Char('o') => Some(Msg::OpenInBrowser),
        KeyCode::Char('?') => Some(Msg::ToggleHelp),
        KeyCode::Char('f') if *screen == Screen::Board => Some(Msg::OpenFilterPicker),
        KeyCode::Char('p') if *screen == Screen::Board => Some(Msg::OpenRank),
        KeyCode::Char('a') if *screen == Screen::Board => Some(Msg::AuditAction),
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
                map_key(&Screen::Board, false, false, false, false, key),
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
                    map_key(&screen, false, false, false, false, key),
                    Some(expected.clone())
                );
            }
        }
    }

    #[test]
    fn enter_maps_to_enter_on_every_screen() {
        for screen in [Screen::Board, Screen::Detail, Screen::TransitionMenu] {
            assert_eq!(
                map_key(&screen, false, false, false, false, KeyCode::Enter),
                Some(Msg::Enter)
            );
        }
    }

    #[test]
    fn esc_and_q_map_to_back_on_every_screen() {
        for screen in [Screen::Board, Screen::Detail, Screen::TransitionMenu] {
            assert_eq!(
                map_key(&screen, false, false, false, false, KeyCode::Esc),
                Some(Msg::Back)
            );
            assert_eq!(
                map_key(&screen, false, false, false, false, KeyCode::Char('q')),
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
                KeyCode::Char('r')
            ),
            Some(Msg::Refresh)
        );
    }

    #[test]
    fn o_maps_to_open_in_browser() {
        assert_eq!(
            map_key(
                &Screen::Board,
                false,
                false,
                false,
                false,
                KeyCode::Char('o')
            ),
            Some(Msg::OpenInBrowser)
        );
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
                KeyCode::Char('z')
            ),
            None
        );
        assert_eq!(
            map_key(&Screen::Board, false, false, false, false, KeyCode::Tab),
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
                KeyCode::Char('z')
            ),
            Some(Msg::ToggleHelp)
        );
        assert_eq!(
            map_key(&Screen::Board, true, false, false, false, KeyCode::Enter),
            Some(Msg::ToggleHelp)
        );
        assert_eq!(
            map_key(
                &Screen::Board,
                true,
                false,
                false,
                false,
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
                KeyCode::Char('f')
            ),
            Some(Msg::OpenFilterPicker)
        );
    }

    #[test]
    fn f_is_unbound_off_the_board_screen() {
        for screen in [Screen::Detail, Screen::TransitionMenu] {
            assert_eq!(
                map_key(&screen, false, false, false, false, KeyCode::Char('f')),
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
                map_key(&Screen::Board, false, true, false, false, key),
                Some(expected)
            );
        }
    }

    #[test]
    fn filter_picker_enter_selects() {
        assert_eq!(
            map_key(&Screen::Board, false, true, false, false, KeyCode::Enter),
            Some(Msg::FilterPickerSelect)
        );
    }

    #[test]
    fn filter_picker_esc_and_q_close_it() {
        assert_eq!(
            map_key(&Screen::Board, false, true, false, false, KeyCode::Esc),
            Some(Msg::FilterPickerClose)
        );
        assert_eq!(
            map_key(
                &Screen::Board,
                false,
                true,
                false,
                false,
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
                KeyCode::Char('p')
            ),
            Some(Msg::OpenRank)
        );
    }

    #[test]
    fn p_is_unbound_off_the_board_screen() {
        for screen in [Screen::Detail, Screen::TransitionMenu, Screen::Rank] {
            assert_eq!(
                map_key(&screen, false, false, false, false, KeyCode::Char('p')),
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
                map_key(&screen, false, false, false, false, KeyCode::Char('a')),
                None
            );
        }
    }

    #[test]
    fn enter_and_space_grab_toggle_on_rank_screen() {
        assert_eq!(
            map_key(&Screen::Rank, false, false, false, false, KeyCode::Enter),
            Some(Msg::RankGrabToggle)
        );
        assert_eq!(
            map_key(
                &Screen::Rank,
                false,
                false,
                false,
                false,
                KeyCode::Char(' ')
            ),
            Some(Msg::RankGrabToggle)
        );
    }

    #[test]
    fn space_is_unbound_off_the_rank_screen() {
        for screen in [Screen::Board, Screen::Detail, Screen::TransitionMenu] {
            assert_eq!(
                map_key(&screen, false, false, false, false, KeyCode::Char(' ')),
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
                KeyCode::Char('k')
            ),
            Some(Msg::Up)
        );
        assert_eq!(
            map_key(&Screen::Rank, false, false, false, false, KeyCode::Esc),
            Some(Msg::Back)
        );
        assert_eq!(
            map_key(
                &Screen::Rank,
                false,
                false,
                false,
                false,
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
                KeyCode::Char('?')
            ),
            Some(Msg::ToggleHelp)
        );
    }

    #[test]
    fn r_is_inert_on_rank_screen_while_a_ticket_is_grabbed() {
        assert_eq!(
            map_key(&Screen::Rank, false, false, true, false, KeyCode::Char('r')),
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
                map_key(&Screen::Rank, false, false, true, false, key),
                map_key(&Screen::Rank, false, false, false, false, key),
                "key {key:?} should behave the same grabbed or not"
            );
        }
    }

    #[test]
    fn rank_grabbed_flag_is_ignored_off_the_rank_screen() {
        for screen in [Screen::Board, Screen::Detail, Screen::TransitionMenu] {
            assert_eq!(
                map_key(&screen, false, false, true, false, KeyCode::Char('r')),
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
                map_key(&Screen::Board, false, true, false, false, key),
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
                KeyCode::Char('k')
            ),
            Some(Msg::Up)
        );
        assert_eq!(
            map_key(&Screen::Runs, false, false, false, false, KeyCode::Enter),
            Some(Msg::Enter)
        );
        assert_eq!(
            map_key(
                &Screen::Runs,
                false,
                false,
                false,
                false,
                KeyCode::Char('r')
            ),
            Some(Msg::Refresh)
        );
        assert_eq!(
            map_key(&Screen::Runs, false, false, false, false, KeyCode::Esc),
            Some(Msg::Back)
        );
        assert_eq!(
            map_key(
                &Screen::Runs,
                false,
                false,
                false,
                false,
                KeyCode::Char('q')
            ),
            Some(Msg::Back)
        );
    }

    #[test]
    fn run_detail_open_restricts_to_scroll_close_and_refresh() {
        assert_eq!(
            map_key(&Screen::Runs, false, false, false, true, KeyCode::Char('j')),
            Some(Msg::Down)
        );
        assert_eq!(
            map_key(&Screen::Runs, false, false, false, true, KeyCode::Down),
            Some(Msg::Down)
        );
        assert_eq!(
            map_key(&Screen::Runs, false, false, false, true, KeyCode::Char('k')),
            Some(Msg::Up)
        );
        assert_eq!(
            map_key(&Screen::Runs, false, false, false, true, KeyCode::Up),
            Some(Msg::Up)
        );
        assert_eq!(
            map_key(&Screen::Runs, false, false, false, true, KeyCode::Esc),
            Some(Msg::Back)
        );
        assert_eq!(
            map_key(&Screen::Runs, false, false, false, true, KeyCode::Char('q')),
            Some(Msg::Back)
        );
        assert_eq!(
            map_key(&Screen::Runs, false, false, false, true, KeyCode::Char('r')),
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
            assert_eq!(map_key(&Screen::Runs, false, false, false, true, key), None);
        }
    }

    #[test]
    fn show_run_detail_flag_is_ignored_off_the_runs_screen() {
        for screen in [Screen::Board, Screen::Detail, Screen::TransitionMenu] {
            assert_eq!(
                map_key(&screen, false, false, false, true, KeyCode::Char('r')),
                Some(Msg::Refresh)
            );
        }
    }
}
