//! Keybindings: maps a raw key press to a [`Msg`], independent of terminal
//! I/O so the bindings can be tested without a real terminal.

use crossterm::event::KeyCode;

use crate::tui::app::{Msg, Screen};

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
pub fn map_key(
    screen: &Screen,
    show_help: bool,
    show_filter_picker: bool,
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
            assert_eq!(map_key(&Screen::Board, false, false, key), Some(expected));
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
                assert_eq!(map_key(&screen, false, false, key), Some(expected.clone()));
            }
        }
    }

    #[test]
    fn enter_maps_to_enter_on_every_screen() {
        for screen in [Screen::Board, Screen::Detail, Screen::TransitionMenu] {
            assert_eq!(
                map_key(&screen, false, false, KeyCode::Enter),
                Some(Msg::Enter)
            );
        }
    }

    #[test]
    fn esc_and_q_map_to_back_on_every_screen() {
        for screen in [Screen::Board, Screen::Detail, Screen::TransitionMenu] {
            assert_eq!(
                map_key(&screen, false, false, KeyCode::Esc),
                Some(Msg::Back)
            );
            assert_eq!(
                map_key(&screen, false, false, KeyCode::Char('q')),
                Some(Msg::Back)
            );
        }
    }

    #[test]
    fn r_maps_to_refresh() {
        assert_eq!(
            map_key(&Screen::Board, false, false, KeyCode::Char('r')),
            Some(Msg::Refresh)
        );
    }

    #[test]
    fn o_maps_to_open_in_browser() {
        assert_eq!(
            map_key(&Screen::Board, false, false, KeyCode::Char('o')),
            Some(Msg::OpenInBrowser)
        );
    }

    #[test]
    fn question_mark_maps_to_toggle_help() {
        assert_eq!(
            map_key(&Screen::Board, false, false, KeyCode::Char('?')),
            Some(Msg::ToggleHelp)
        );
    }

    #[test]
    fn unbound_key_maps_to_none() {
        assert_eq!(
            map_key(&Screen::Board, false, false, KeyCode::Char('z')),
            None
        );
        assert_eq!(map_key(&Screen::Board, false, false, KeyCode::Tab), None);
    }

    #[test]
    fn help_overlay_swallows_any_key_and_closes_it() {
        assert_eq!(
            map_key(&Screen::Board, true, false, KeyCode::Char('z')),
            Some(Msg::ToggleHelp)
        );
        assert_eq!(
            map_key(&Screen::Board, true, false, KeyCode::Enter),
            Some(Msg::ToggleHelp)
        );
        assert_eq!(
            map_key(&Screen::Board, true, false, KeyCode::Char('r')),
            Some(Msg::ToggleHelp)
        );
    }

    #[test]
    fn help_overlay_q_quits_instead_of_closing() {
        assert_eq!(
            map_key(&Screen::Board, true, false, KeyCode::Char('q')),
            Some(Msg::Quit)
        );
    }

    #[test]
    fn f_opens_filter_picker_on_board_screen() {
        assert_eq!(
            map_key(&Screen::Board, false, false, KeyCode::Char('f')),
            Some(Msg::OpenFilterPicker)
        );
    }

    #[test]
    fn f_is_unbound_off_the_board_screen() {
        for screen in [Screen::Detail, Screen::TransitionMenu] {
            assert_eq!(map_key(&screen, false, false, KeyCode::Char('f')), None);
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
            assert_eq!(map_key(&Screen::Board, false, true, key), Some(expected));
        }
    }

    #[test]
    fn filter_picker_enter_selects() {
        assert_eq!(
            map_key(&Screen::Board, false, true, KeyCode::Enter),
            Some(Msg::FilterPickerSelect)
        );
    }

    #[test]
    fn filter_picker_esc_and_q_close_it() {
        assert_eq!(
            map_key(&Screen::Board, false, true, KeyCode::Esc),
            Some(Msg::FilterPickerClose)
        );
        assert_eq!(
            map_key(&Screen::Board, false, true, KeyCode::Char('q')),
            Some(Msg::FilterPickerClose)
        );
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
            assert_eq!(map_key(&Screen::Board, false, true, key), None);
        }
    }
}
