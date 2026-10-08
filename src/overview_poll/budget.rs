//! The overview's refresh budget (ADR-0009 decision 3): when each scope is
//! due a fetch, and what the view shows for it between fetches.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::snapshot::ScopeSnapshot;

/// How often each scope is fetched unless forced.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(60);

/// When each scope was last fetched, and so which are due.
#[derive(Debug, Clone)]
pub struct RefreshSchedule {
    interval: Duration,
    last: HashMap<String, Instant>,
}

impl RefreshSchedule {
    /// A schedule fetching each scope at most once per `interval`.
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: HashMap::new(),
        }
    }

    /// The scopes in `scopes` due a fetch at `now`: never fetched, or last
    /// fetched at least one interval ago. Order follows `scopes`.
    pub fn due(&self, scopes: &[String], now: Instant) -> Vec<String> {
        scopes
            .iter()
            .filter(|scope| match self.last.get(*scope) {
                Some(last) => now.saturating_duration_since(*last) >= self.interval,
                None => true,
            })
            .cloned()
            .collect()
    }

    /// Record that `scope` was fetched (successfully or not) at `now`. A
    /// failed fetch also waits a full interval, so a broken scope can't
    /// spend the budget of every other.
    pub fn mark(&mut self, scope: &str, now: Instant) {
        self.last.insert(scope.to_string(), now);
    }

    /// Make every scope due on the next [`RefreshSchedule::due`] (`r`).
    pub fn force(&mut self) {
        self.last.clear();
    }
}

/// What the view knows about one scope.
#[derive(Debug, Clone, Default)]
pub struct ScopeState {
    /// The last successful fetch, if any. Kept across failures.
    pub snapshot: Option<ScopeSnapshot>,
    /// When `snapshot` was fetched.
    pub fetched_at: Option<Instant>,
    /// The latest fetch's error, if it failed. `Some` marks the project
    /// strip entry stale; it clears on the next success.
    pub error: Option<String>,
}

impl ScopeState {
    /// Whether the latest fetch failed.
    pub fn is_stale(&self) -> bool {
        self.error.is_some()
    }

    /// Age of the last good snapshot at `now`, if there is one.
    pub fn age(&self, now: Instant) -> Option<Duration> {
        self.fetched_at
            .map(|fetched_at| now.saturating_duration_since(fetched_at))
    }
}

/// Every scope's [`ScopeState`], fed by the poller's results.
#[derive(Debug, Clone, Default)]
pub struct Snapshots {
    scopes: HashMap<String, ScopeState>,
}

impl Snapshots {
    /// Record a fetch of `scope` that finished at `now`. Success replaces
    /// the snapshot and clears the error; failure keeps the last good
    /// snapshot and only sets the error, so the view never blanks.
    pub fn apply(&mut self, scope: &str, result: Result<ScopeSnapshot, String>, now: Instant) {
        let state = self.scopes.entry(scope.to_string()).or_default();
        match result {
            Ok(snapshot) => {
                state.snapshot = Some(snapshot);
                state.fetched_at = Some(now);
                state.error = None;
            }
            Err(error) => state.error = Some(error),
        }
    }

    /// `scope`'s state, if it has ever been fetched.
    pub fn get(&self, scope: &str) -> Option<&ScopeState> {
        self.scopes.get(scope)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overview_poll::snapshot::ReadyTicket;

    fn scopes(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn unfetched_scopes_are_due_and_fetched_ones_wait_an_interval() {
        let start = Instant::now();
        let mut schedule = RefreshSchedule::new(Duration::from_secs(60));
        let all = scopes(&["a", "b"]);

        assert_eq!(schedule.due(&all, start), all);

        schedule.mark("a", start);
        assert_eq!(schedule.due(&all, start), scopes(&["b"]));
        assert_eq!(
            schedule.due(&all, start + Duration::from_secs(59)),
            scopes(&["b"])
        );
        assert_eq!(schedule.due(&all, start + Duration::from_secs(60)), all);
    }

    #[test]
    fn force_makes_every_scope_due_now() {
        let start = Instant::now();
        let mut schedule = RefreshSchedule::new(Duration::from_secs(60));
        let all = scopes(&["a", "b"]);
        schedule.mark("a", start);
        schedule.mark("b", start);

        schedule.force();

        assert_eq!(schedule.due(&all, start), all);
    }

    fn snapshot_with_ready(key: &str) -> ScopeSnapshot {
        ScopeSnapshot {
            ready: vec![ReadyTicket {
                key: key.to_string(),
                summary: String::new(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn a_failed_fetch_keeps_the_last_good_snapshot_and_marks_it_stale() {
        let start = Instant::now();
        let later = start + Duration::from_secs(60);
        let mut snapshots = Snapshots::default();

        snapshots.apply("a", Ok(snapshot_with_ready("GH-1")), start);
        snapshots.apply("a", Err("gh: timed out".to_string()), later);

        let state = snapshots.get("a").unwrap();
        assert!(state.is_stale());
        assert_eq!(state.snapshot, Some(snapshot_with_ready("GH-1")));
        assert_eq!(state.age(later), Some(Duration::from_secs(60)));
    }

    #[test]
    fn a_successful_fetch_replaces_the_snapshot_and_clears_staleness() {
        let start = Instant::now();
        let mut snapshots = Snapshots::default();
        snapshots.apply("a", Err("boom".to_string()), start);
        assert!(snapshots.get("a").unwrap().snapshot.is_none());

        snapshots.apply("a", Ok(snapshot_with_ready("GH-2")), start);

        let state = snapshots.get("a").unwrap();
        assert!(!state.is_stale());
        assert_eq!(state.snapshot, Some(snapshot_with_ready("GH-2")));
    }
}
