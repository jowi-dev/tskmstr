//! `tm work`: port of devtools' `j work` lane runner (see
//! `docs/plans/runner-port.md`). `new`/`remove`/`list`/`restore`/`start`
//! and both the foreground (`--fg`) and detached (default) `run` paths are
//! wired into the CLI (`src/cli/work.rs`, steps 5, 9, and 10). See
//! [`detach`] for the detached path's design.

pub mod audit;
pub mod bugbot;
pub mod claude;
pub mod detach;
pub mod git;
pub mod hooks;
pub mod hooks_install;
pub mod naming;
pub mod review_watch;
pub mod run;
pub mod runner;
pub mod tmux;
