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
pub fn map_key(_screen: &Screen, show_help: bool, key: KeyCode) -> Option<Msg> {
    if show_help {
        return Some(match key {
            KeyCode::Char('q') => Msg::Quit,
            _ => Msg::ToggleHelp,
        });
    }

    match key {
        KeyCode::Char('j') | KeyCode::Down => Some(Msg::Down),
        KeyCode::Char('k') | KeyCode::Up => Some(Msg::Up),
        KeyCode::Enter => Some(Msg::Enter),
        KeyCode::Esc | KeyCode::Char('q') => Some(Msg::Back),
        KeyCode::Char('r') => Some(Msg::Refresh),
        KeyCode::Char('o') => Some(Msg::OpenInBrowser),
        KeyCode::Char('?') => Some(Msg::ToggleHelp),
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
            assert_eq!(map_key(&Screen::Board, false, key), Some(expected));
        }
    }

    #[test]
    fn enter_maps_to_enter_on_every_screen() {
        for screen in [Screen::Board, Screen::Detail, Screen::TransitionMenu] {
            assert_eq!(map_key(&screen, false, KeyCode::Enter), Some(Msg::Enter));
        }
    }

    #[test]
    fn esc_and_q_map_to_back_on_every_screen() {
        for screen in [Screen::Board, Screen::Detail, Screen::TransitionMenu] {
            assert_eq!(map_key(&screen, false, KeyCode::Esc), Some(Msg::Back));
            assert_eq!(map_key(&screen, false, KeyCode::Char('q')), Some(Msg::Back));
        }
    }

    #[test]
    fn r_maps_to_refresh() {
        assert_eq!(
            map_key(&Screen::Board, false, KeyCode::Char('r')),
            Some(Msg::Refresh)
        );
    }

    #[test]
    fn o_maps_to_open_in_browser() {
        assert_eq!(
            map_key(&Screen::Board, false, KeyCode::Char('o')),
            Some(Msg::OpenInBrowser)
        );
    }

    #[test]
    fn question_mark_maps_to_toggle_help() {
        assert_eq!(
            map_key(&Screen::Board, false, KeyCode::Char('?')),
            Some(Msg::ToggleHelp)
        );
    }

    #[test]
    fn unbound_key_maps_to_none() {
        assert_eq!(map_key(&Screen::Board, false, KeyCode::Char('z')), None);
        assert_eq!(map_key(&Screen::Board, false, KeyCode::Tab), None);
    }

    #[test]
    fn help_overlay_swallows_any_key_and_closes_it() {
        assert_eq!(
            map_key(&Screen::Board, true, KeyCode::Char('z')),
            Some(Msg::ToggleHelp)
        );
        assert_eq!(
            map_key(&Screen::Board, true, KeyCode::Enter),
            Some(Msg::ToggleHelp)
        );
        assert_eq!(
            map_key(&Screen::Board, true, KeyCode::Char('r')),
            Some(Msg::ToggleHelp)
        );
    }

    #[test]
    fn help_overlay_q_quits_instead_of_closing() {
        assert_eq!(
            map_key(&Screen::Board, true, KeyCode::Char('q')),
            Some(Msg::Quit)
        );
    }
}
