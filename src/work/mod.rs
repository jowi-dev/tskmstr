//! `tm work`: port of devtools' `j work` lane runner (see
//! `docs/plans/runner-port.md`). `new`/`remove`/`list`/`restore`/`start`
//! and both the foreground (`--fg`) and detached (default) `run` paths are
//! wired into the CLI (`src/cli/work.rs`, steps 5, 9, and 10). See
//! [`detach`] for the detached path's design.

pub mod admission;
pub mod audit;
pub mod bugbot;
pub mod build_slots;
pub mod create;
pub mod detach;
pub mod git;
pub mod hibernate;
pub mod interactive;
pub mod kill_safety;
pub mod manual;
pub mod merge;
pub mod naming;
pub mod prompt;
pub mod review_watch;
pub mod run;
pub mod runner;
pub mod session;
pub mod session_gc;
pub mod tmux;
pub mod vdiff;
pub mod viewer;
