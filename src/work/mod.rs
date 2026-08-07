//! `tm work`: port of devtools' `j work` lane runner (see
//! `docs/plans/runner-port.md`). `new`/`remove`/`list`/`restore`/`start`
//! and the foreground `run` path are wired into the CLI (`src/cli/work.rs`,
//! steps 5 and 9); detached `run` (step 10) is not yet.

pub mod claude;
pub mod git;
pub mod hooks;
pub mod naming;
pub mod run;
pub mod runner;
pub mod tmux;
