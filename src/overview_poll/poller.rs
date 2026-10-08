//! The background thread that runs scope fetches off the render thread.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use super::budget::{RefreshSchedule, Snapshots};
use super::snapshot::{ScopeSnapshot, ScopeTarget};

/// How often the poller thread wakes to check for due scopes when no
/// command arrives. Matches the TUI loop's 250 ms tick.
const TICK: Duration = Duration::from_millis(250);

/// Fetches one scope. Production wraps [`super::snapshot::fetch_scope`]
/// with a `gh` client and the scope's own ticket provider, built inside the
/// call since neither is `Send`.
pub trait ScopeFetcher: Send + 'static {
    /// Fetch `target`, rendering any error as a message for the strip.
    fn fetch(&self, target: &ScopeTarget) -> Result<ScopeSnapshot, String>;
}

impl<F> ScopeFetcher for F
where
    F: Fn(&ScopeTarget) -> Result<ScopeSnapshot, String> + Send + 'static,
{
    fn fetch(&self, target: &ScopeTarget) -> Result<ScopeSnapshot, String> {
        self(target)
    }
}

enum Command {
    Targets(Vec<ScopeTarget>),
    Refresh,
}

struct FetchResult {
    scope: String,
    result: Result<ScopeSnapshot, String>,
    finished_at: Instant,
}

/// Handle to the background poller. Dropping it stops the thread after its
/// current fetch; it is never joined, so quitting the view doesn't wait on
/// a slow `gh`.
pub struct Poller {
    commands: Sender<Command>,
    results: Receiver<FetchResult>,
}

impl Poller {
    /// Start the poller thread, fetching each target at most once per
    /// `interval` (see [`super::budget::REFRESH_INTERVAL`]). It fetches
    /// nothing until [`Poller::set_targets`] is called.
    pub fn spawn(fetcher: impl ScopeFetcher, interval: Duration) -> Self {
        let (commands, command_rx) = mpsc::channel();
        let (result_tx, results) = mpsc::channel();
        thread::spawn(move || run(fetcher, interval, command_rx, result_tx));
        Self { commands, results }
    }

    /// Replace the set of scopes to poll. New scopes are fetched on the
    /// thread's next wake; dropped ones stop being fetched.
    pub fn set_targets(&self, targets: Vec<ScopeTarget>) {
        let _ = self.commands.send(Command::Targets(targets));
    }

    /// Fetch every target now, regardless of the interval (`r`).
    pub fn refresh(&self) {
        let _ = self.commands.send(Command::Refresh);
    }

    /// Apply every finished fetch to `snapshots` without blocking. Returns
    /// how many were applied. Call once per render tick.
    pub fn drain_into(&self, snapshots: &mut Snapshots) -> usize {
        let mut applied = 0;
        while let Ok(fetched) = self.results.try_recv() {
            snapshots.apply(&fetched.scope, fetched.result, fetched.finished_at);
            applied += 1;
        }
        applied
    }
}

/// The poller thread: wait for a command or the next tick, then fetch every
/// due target in turn, one fetch (one batched set of calls) per repo.
/// Exits once the [`Poller`] is dropped.
fn run(
    fetcher: impl ScopeFetcher,
    interval: Duration,
    commands: Receiver<Command>,
    results: Sender<FetchResult>,
) {
    let mut targets: Vec<ScopeTarget> = Vec::new();
    let mut schedule = RefreshSchedule::new(interval);
    loop {
        match commands.recv_timeout(TICK) {
            Ok(command) => apply(command, &mut targets, &mut schedule),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        while let Ok(command) = commands.try_recv() {
            apply(command, &mut targets, &mut schedule);
        }

        let scopes: Vec<String> = targets.iter().map(|t| t.scope.clone()).collect();
        for scope in schedule.due(&scopes, Instant::now()) {
            let Some(target) = targets.iter().find(|t| t.scope == scope) else {
                continue;
            };
            let result = fetcher.fetch(target);
            let finished_at = Instant::now();
            schedule.mark(&scope, finished_at);
            let fetched = FetchResult {
                scope,
                result,
                finished_at,
            };
            if results.send(fetched).is_err() {
                return;
            }
        }
    }
}

fn apply(command: Command, targets: &mut Vec<ScopeTarget>, schedule: &mut RefreshSchedule) {
    match command {
        Command::Targets(new) => *targets = new,
        Command::Refresh => schedule.force(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use super::*;

    fn target(scope: &str) -> ScopeTarget {
        ScopeTarget {
            scope: scope.to_string(),
            repo_root: PathBuf::from("/repo"),
            tracked: Vec::new(),
        }
    }

    /// Drain until `count` results have been applied in total, or fail
    /// after a few seconds.
    fn drain_until(poller: &Poller, snapshots: &mut Snapshots, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut applied = 0;
        while applied < count {
            assert!(
                Instant::now() < deadline,
                "only {applied} of {count} results"
            );
            applied += poller.drain_into(snapshots);
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// A fetcher that records every scope it's asked for and fails `fail`.
    struct Recording {
        calls: Arc<Mutex<Vec<String>>>,
        fail: &'static str,
    }

    impl ScopeFetcher for Recording {
        fn fetch(&self, target: &ScopeTarget) -> Result<ScopeSnapshot, String> {
            self.calls.lock().unwrap().push(target.scope.clone());
            if target.scope == self.fail {
                Err("gh: timed out".to_string())
            } else {
                Ok(ScopeSnapshot::default())
            }
        }
    }

    fn recording_fetcher(fail: &'static str) -> (Arc<Mutex<Vec<String>>>, Recording) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let fetcher = Recording {
            calls: Arc::clone(&calls),
            fail,
        };
        (calls, fetcher)
    }

    #[test]
    fn fetches_each_target_once_per_interval_and_again_on_refresh() {
        let (calls, fetcher) = recording_fetcher("");
        let poller = Poller::spawn(fetcher, Duration::from_secs(60));
        let mut snapshots = Snapshots::default();

        poller.set_targets(vec![target("a"), target("b")]);
        drain_until(&poller, &mut snapshots, 2);
        thread::sleep(TICK * 3);
        assert_eq!(
            *calls.lock().unwrap(),
            ["a", "b"],
            "no refetch inside the interval"
        );
        assert!(snapshots.get("a").unwrap().snapshot.is_some());

        poller.refresh();
        drain_until(&poller, &mut snapshots, 2);
        assert_eq!(*calls.lock().unwrap(), ["a", "b", "a", "b"]);
    }

    #[test]
    fn a_failing_scope_is_marked_stale_without_affecting_others() {
        let (_calls, fetcher) = recording_fetcher("b");
        let poller = Poller::spawn(fetcher, Duration::from_secs(60));
        let mut snapshots = Snapshots::default();

        poller.set_targets(vec![target("a"), target("b")]);
        drain_until(&poller, &mut snapshots, 2);

        assert!(!snapshots.get("a").unwrap().is_stale());
        assert!(snapshots.get("b").unwrap().is_stale());
    }
}
