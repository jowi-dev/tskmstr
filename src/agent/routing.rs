//! Agent dispatch and priority-routing selection logic for GitHub issue
//! #54 (`docs/plans/gh-54-priority-routing.md`).
//!
//! [`runner_for`] is the one factory that turns a [`crate::config::AgentKind`]
//! into a live [`AgentRunner`] — moved here from `main.rs`'s
//! `agent_runner_for` (see `docs/decisions/0003-ticket-providers.md`'s
//! one-enum-one-dispatch precedent, mirrored by
//! `docs/decisions/0004-agent-runners.md`) so it lives next to the adapters
//! it dispatches to, alongside [`plan_attempts`], the fallback-order
//! selection logic `src/work/run.rs`'s lane-run prepare/run path consults.

use crate::agent::AgentRunner;
use crate::agent::claude::ClaudeRunner;
use crate::agent::opencode::OpencodeRunner;
use crate::config::AgentKind;

/// Build an [`AgentRunner`] for `kind`, mirroring
/// `crate::ticketing::ticket_provider_for`: the one factory that turns
/// [`AgentKind`] into a live implementation, so nothing else outside config
/// parsing needs to `match` on it (see `docs/plans/agent-runner.md` and
/// GitHub issue #17).
///
/// Each arm leaks a freshly constructed runner to get a
/// `&'static dyn AgentRunner` — every [`AgentRunner`] implementation so far
/// is a zero-sized unit struct and `tm` is a short-lived CLI process, so
/// leaking one costs nothing, the same trade `ticket_provider_for` makes for
/// `ShellGhCli`.
pub fn runner_for(kind: AgentKind) -> &'static dyn AgentRunner {
    match kind {
        AgentKind::Claude => Box::leak(Box::new(ClaudeRunner)),
        AgentKind::Opencode => Box::leak(Box::new(OpencodeRunner)),
    }
}

/// How long to hold an agent's usage-limit window closed when the
/// rate-limit outcome that triggered it carried no reset timestamp.
///
/// 15 minutes: re-probing an agent this often is cheap because the run
/// still completes on the fallback agent — a premature probe costs one
/// spawn, not the run. See `docs/plans/gh-54-priority-routing.md`'s "Window
/// persistence" section.
pub const DEFAULT_EXHAUSTED_HOLD_SECS: i64 = 900;

/// Filters `order` down to the agents whose usage-limit window has passed
/// (or was never recorded), preserving `order`'s relative sequence, for
/// `src/work/run.rs`'s lane-run prepare path to build one invocation per
/// surviving attempt.
///
/// `exhausted_until` is keyed by [`AgentRunner::name`] (e.g. `"claude"`),
/// mirroring how `runs.db`'s `agent_windows` table stores it, and returns
/// the agent's recorded exhausted-until timestamp (unix seconds), or `None`
/// if it has never been recorded (or has since been cleared).
///
/// If every agent in `order` is still exhausted, returns a single-element
/// vec holding whichever agent's window closes soonest — probing the
/// soonest-recovering agent rather than refusing to run at all (see the
/// plan doc's "Selection and in-run fallback" section).
///
/// `order` must be non-empty; an empty `order` returns an empty vec (no
/// agent to probe).
pub fn plan_attempts(
    order: &[AgentKind],
    exhausted_until: impl Fn(&str) -> Option<i64>,
    now: i64,
) -> Vec<AgentKind> {
    let available: Vec<AgentKind> = order
        .iter()
        .copied()
        .filter(|kind| match exhausted_until(runner_for(*kind).name()) {
            Some(until) => until <= now,
            None => true,
        })
        .collect();

    if !available.is_empty() {
        return available;
    }

    // Every agent in `order` is still exhausted (or `order` was empty to
    // begin with): probe whichever one recovers soonest instead of refusing
    // to run. `min_by_key` on a missing `exhausted_until` (i.e. `None`)
    // can't happen here since `available` would have included it above, but
    // treat it as "recovers now" (`i64::MIN`) rather than panicking, just in
    // case a caller races a clear against this read.
    order
        .iter()
        .copied()
        .min_by_key(|kind| exhausted_until(runner_for(*kind).name()).unwrap_or(i64::MIN))
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runner_for_claude_returns_the_claude_runner() {
        assert_eq!(runner_for(AgentKind::Claude).name(), "claude");
    }

    #[test]
    fn runner_for_opencode_returns_the_opencode_runner() {
        assert_eq!(runner_for(AgentKind::Opencode).name(), "opencode");
    }

    #[test]
    fn plan_attempts_with_no_windows_returns_the_full_order() {
        let order = vec![AgentKind::Claude, AgentKind::Opencode];
        let attempts = plan_attempts(&order, |_agent| None, 1_000);
        assert_eq!(attempts, vec![AgentKind::Claude, AgentKind::Opencode]);
    }

    #[test]
    fn plan_attempts_skips_the_first_agent_when_its_window_is_still_open() {
        let order = vec![AgentKind::Claude, AgentKind::Opencode];
        let attempts = plan_attempts(
            &order,
            |agent| if agent == "claude" { Some(2_000) } else { None },
            1_000,
        );
        assert_eq!(attempts, vec![AgentKind::Opencode]);
    }

    #[test]
    fn plan_attempts_includes_an_agent_whose_window_closed_exactly_at_now() {
        let order = vec![AgentKind::Claude, AgentKind::Opencode];
        let attempts = plan_attempts(
            &order,
            |agent| if agent == "claude" { Some(1_000) } else { None },
            1_000,
        );
        assert_eq!(attempts, vec![AgentKind::Claude, AgentKind::Opencode]);
    }

    #[test]
    fn plan_attempts_all_exhausted_returns_the_single_earliest_resetting_agent() {
        let order = vec![AgentKind::Claude, AgentKind::Opencode];
        let attempts = plan_attempts(
            &order,
            |agent| {
                if agent == "claude" {
                    Some(5_000)
                } else {
                    Some(3_000)
                }
            },
            1_000,
        );
        assert_eq!(attempts, vec![AgentKind::Opencode]);
    }

    #[test]
    fn plan_attempts_all_exhausted_prefers_earlier_order_entry_on_a_tie() {
        let order = vec![AgentKind::Claude, AgentKind::Opencode];
        let attempts = plan_attempts(&order, |_agent| Some(5_000), 1_000);
        assert_eq!(attempts, vec![AgentKind::Claude]);
    }

    #[test]
    fn plan_attempts_single_agent_order_with_no_window_returns_it() {
        let order = vec![AgentKind::Claude];
        let attempts = plan_attempts(&order, |_agent| None, 1_000);
        assert_eq!(attempts, vec![AgentKind::Claude]);
    }

    #[test]
    fn plan_attempts_single_agent_order_still_exhausted_returns_it_anyway() {
        let order = vec![AgentKind::Claude];
        let attempts = plan_attempts(&order, |_agent| Some(5_000), 1_000);
        assert_eq!(attempts, vec![AgentKind::Claude]);
    }

    #[test]
    fn plan_attempts_single_agent_order_window_expired_returns_it() {
        let order = vec![AgentKind::Claude];
        let attempts = plan_attempts(&order, |_agent| Some(500), 1_000);
        assert_eq!(attempts, vec![AgentKind::Claude]);
    }
}
