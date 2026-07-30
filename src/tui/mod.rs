//! Interactive terminal board: an Elm-style TUI (state, messages, a pure
//! reducer, rendering, and keybindings) built so that all logic except the
//! final terminal wiring is testable without a real terminal.

pub mod app;
pub mod keymap;
pub mod ui;
