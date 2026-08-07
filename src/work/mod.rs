//! `tm work`: port of devtools' `j work` lane runner (see
//! `docs/plans/runner-port.md`). `new`/`remove`/`list`/`restore`/`start`
//! are wired into the CLI (`src/cli/work.rs`, step 5); `run` and hook
//! deployment (steps 6+) are not yet.

pub mod claude;
pub mod git;
pub mod hooks;
pub mod naming;
pub mod tmux;
