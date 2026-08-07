//! `tm work`: port of devtools' `j work` lane runner (see
//! `docs/plans/runner-port.md`). Building out incrementally; nothing in
//! here is wired into the CLI yet.
// TODO(runner-port step 5): remove this module-level allow once `tm work`
// subcommands wire `naming` (and friends) into the CLI dispatch.
#![allow(dead_code)]

pub mod git;
pub mod naming;
pub mod tmux;
