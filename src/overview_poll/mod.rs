//! Background tracker and PR state poller for the cross-project overview
//! (ADR-0009 decision 3, GitHub issue #84).
//!
//! The overview's run signals come from `runs.db` on every tick. This
//! module supplies the rest: per scope, which open PRs need review, are
//! ready to merge, or are conflicted ([`signal`]); each tracked ticket's
//! tracker status, for the drift glyph; and the top of the operator's
//! ready list, for "Not started" ([`snapshot`]).
//!
//! The refresh budget is fixed by the ADR, not left to callers:
//!
//! - Only scopes with a live run or a run that ended in the last
//!   [`POLL_WINDOW_DAYS`] days are polled
//!   ([`crate::runs::RunStore::recent_scope_activity`] fed through
//!   [`snapshot::targets_from_activity`]).
//! - Each is fetched at most once per [`budget::REFRESH_INTERVAL`], with one
//!   batched call per kind of data, never one per ticket
//!   ([`snapshot::fetch_scope`]); [`poller::Poller::refresh`] (`r`) forces
//!   every scope due now.
//! - Fetches run on a background thread ([`poller::Poller`]); the view
//!   drains finished fetches into [`budget::Snapshots`] each tick and
//!   renders the last good snapshot with its age. A scope whose fetch fails
//!   keeps that snapshot and is marked stale.
//!
//! Snapshots live only in memory for the life of the view.

pub mod budget;
pub mod poller;
pub mod signal;
pub mod snapshot;

/// How recently a scope's last run must have ended for it to be polled.
pub const POLL_WINDOW_DAYS: u32 = 7;
