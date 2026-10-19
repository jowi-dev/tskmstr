//! The TUI host for the autopilot dispatcher (GitHub issue #116).
//!
//! `tm board` and `tm overview` each start an [`AutopilotHost`]: a
//! background thread that runs a [`crate::work::dispatch::tick`] for every
//! scope the view shows, at most once per `[dispatch] tick_secs`. A tick
//! reads the tracker, so it never runs on the render thread. The view hands
//! the thread its scopes with [`AutopilotHost::set_targets`] and reads each
//! finished tick with [`AutopilotHost::drain`] on its own loop.
//!
//! The thread keeps no autopilot state of its own. The on/off flag lives in
//! runs.db's `autopilot` table, so every host (two TUIs, or a TUI and the
//! `tm dispatch` timer) reads the same flag, and the dispatch core's scope
//! lock keeps them from launching the same ticket. A tick for a scope whose
//! flag is off returns [`crate::work::dispatch::Skip::Disabled`] at once.
//!
//! [`load_status`] builds what the views show for each scope from runs.db:
//! the flag, the running lane count against `max_lanes`, and the reason the
//! newest tick stopped short, if it did.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use crate::runs::{RunStore, RunStoreError};
use crate::work::dispatch::{REFUSED_EVENT, TickReport};

/// How often the host thread wakes to check for due scopes when no command
/// arrives. Matches the TUI loop's 250 ms tick.
const WAKE: Duration = Duration::from_millis(250);

/// One scope the host ticks, and the repo it ticks it from. The tick loads
/// that repo's config and ticket provider, and launches lanes there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutopilotTarget {
    /// [`crate::config::BackendIdentity::scope`].
    pub scope: String,
    /// The scope's main repo root.
    pub repo_root: PathBuf,
}

/// Runs one dispatch tick for a target. Production builds the tick's
/// dependencies inside the call, since a ticket provider and a run store
/// can't cross threads; tests use a closure.
pub trait ScopeTicker: Send + 'static {
    /// Tick `target`, rendering any error as a message for the view.
    fn tick(&self, target: &AutopilotTarget) -> Result<TickReport, String>;
}

impl<F> ScopeTicker for F
where
    F: Fn(&AutopilotTarget) -> Result<TickReport, String> + Send + 'static,
{
    fn tick(&self, target: &AutopilotTarget) -> Result<TickReport, String> {
        self(target)
    }
}

/// One finished tick, as [`AutopilotHost::drain`] returns it.
#[derive(Debug, Clone, PartialEq)]
pub struct TickOutcome {
    /// The scope that was ticked.
    pub scope: String,
    /// What the tick did, or why it failed.
    pub result: Result<TickReport, String>,
}

enum Command {
    Targets(Vec<AutopilotTarget>),
    TickNow,
}

/// Handle to the host thread. Dropping it stops the thread after its
/// current tick. The thread is never joined, so quitting a view doesn't
/// wait on a slow tracker call.
pub struct AutopilotHost {
    commands: Sender<Command>,
    results: Receiver<TickOutcome>,
}

impl AutopilotHost {
    /// Start the host thread, ticking each target at most once per
    /// `interval`. It ticks nothing until [`AutopilotHost::set_targets`] is
    /// called.
    pub fn spawn(ticker: impl ScopeTicker, interval: Duration) -> Self {
        let (commands, command_rx) = mpsc::channel();
        let (result_tx, results) = mpsc::channel();
        thread::spawn(move || run(ticker, interval, command_rx, result_tx));
        Self { commands, results }
    }

    /// Replace the set of scopes to tick. A new scope is ticked on the
    /// thread's next wake; a dropped one stops being ticked.
    pub fn set_targets(&self, targets: Vec<AutopilotTarget>) {
        let _ = self.commands.send(Command::Targets(targets));
    }

    /// Tick every target on the thread's next wake, regardless of the
    /// interval. A view calls this right after turning autopilot on.
    pub fn tick_now(&self) {
        let _ = self.commands.send(Command::TickNow);
    }

    /// Every tick finished since the last call, without blocking. Call
    /// once per render tick.
    pub fn drain(&self) -> Vec<TickOutcome> {
        self.results.try_iter().collect()
    }
}

/// The host thread: wait for a command or the next wake, then tick every
/// due target in turn. A failed tick is reported and the loop carries on.
/// Exits once the [`AutopilotHost`] is dropped.
fn run(
    ticker: impl ScopeTicker,
    interval: Duration,
    commands: Receiver<Command>,
    results: Sender<TickOutcome>,
) {
    let mut targets: Vec<AutopilotTarget> = Vec::new();
    let mut last_tick: HashMap<String, Instant> = HashMap::new();
    loop {
        match commands.recv_timeout(WAKE) {
            Ok(command) => apply(command, &mut targets, &mut last_tick),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        while let Ok(command) = commands.try_recv() {
            apply(command, &mut targets, &mut last_tick);
        }

        for target in &targets {
            let due = last_tick
                .get(&target.scope)
                .is_none_or(|at| at.elapsed() >= interval);
            if !due {
                continue;
            }
            let result = ticker.tick(target);
            last_tick.insert(target.scope.clone(), Instant::now());
            let outcome = TickOutcome {
                scope: target.scope.clone(),
                result,
            };
            if results.send(outcome).is_err() {
                return;
            }
        }
    }
}

fn apply(
    command: Command,
    targets: &mut Vec<AutopilotTarget>,
    last_tick: &mut HashMap<String, Instant>,
) {
    match command {
        Command::Targets(new) => *targets = new,
        Command::TickNow => last_tick.clear(),
    }
}

/// What a view shows for one scope's autopilot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutopilotStatus {
    /// The scope.
    pub scope: String,
    /// The scope's runs.db autopilot flag.
    pub enabled: bool,
    /// Lane runs running now, across every scope (what `max_lanes` caps).
    pub running_lanes: usize,
    /// `[dispatch] max_lanes`.
    pub max_lanes: usize,
    /// The refusal reason of the scope's newest dispatch event, when that
    /// event is a refusal. `None` once a later tick launched something.
    pub last_refusal: Option<String>,
}

impl AutopilotStatus {
    /// One line for a status bar or header, e.g. `autopilot on · lanes 1/2
    /// · not launching: …`. A tick error, when given, replaces the refusal.
    pub fn summary(&self, tick_error: Option<&str>) -> String {
        if !self.enabled {
            return "autopilot off".to_string();
        }
        let mut line = format!(
            "autopilot on · lanes {}/{}",
            self.running_lanes, self.max_lanes
        );
        if let Some(error) = tick_error {
            line.push_str(&format!(" · tick failed: {error}"));
        } else if let Some(refusal) = &self.last_refusal {
            line.push_str(&format!(" · {refusal}"));
        }
        line
    }
}

/// Reads each of `scopes`' autopilot status from `store`, in order.
///
/// # Errors
///
/// Returns the store's error when any read fails.
pub fn load_status(
    store: &RunStore,
    scopes: &[String],
    max_lanes: usize,
) -> Result<Vec<AutopilotStatus>, RunStoreError> {
    let running_lanes = store.running_lanes()?.len();
    scopes
        .iter()
        .map(|scope| {
            let last_refusal = store
                .dispatch_events(scope, 1)?
                .into_iter()
                .next()
                .filter(|event| event.kind == REFUSED_EVENT)
                .map(|event| event.detail);
            Ok(AutopilotStatus {
                scope: scope.clone(),
                enabled: store.autopilot_enabled(scope)?,
                running_lanes,
                max_lanes,
                last_refusal,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tempfile::tempdir;

    use super::*;
    use crate::work::dispatch::LAUNCH_EVENT;

    fn target(scope: &str) -> AutopilotTarget {
        AutopilotTarget {
            scope: scope.to_string(),
            repo_root: PathBuf::from("/repo"),
        }
    }

    /// Drain until `count` outcomes have arrived in total, or fail after a
    /// few seconds.
    fn drain_until(host: &AutopilotHost, count: usize) -> Vec<TickOutcome> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut outcomes = Vec::new();
        while outcomes.len() < count {
            assert!(
                Instant::now() < deadline,
                "only {} of {count} outcomes",
                outcomes.len()
            );
            outcomes.extend(host.drain());
            thread::sleep(Duration::from_millis(10));
        }
        outcomes
    }

    /// The scopes a [`recording_ticker`] was asked to tick, in order.
    type Calls = Arc<Mutex<Vec<String>>>;

    /// A ticker that records every scope it's asked to tick and fails
    /// `fail`.
    fn recording_ticker(fail: &'static str) -> (Calls, impl ScopeTicker) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&calls);
        let ticker = move |target: &AutopilotTarget| {
            seen.lock().unwrap().push(target.scope.clone());
            if target.scope == fail {
                Err("gh: timed out".to_string())
            } else {
                Ok(TickReport::default())
            }
        };
        (calls, ticker)
    }

    #[test]
    fn ticks_each_target_once_per_interval_and_again_on_tick_now() {
        let (calls, ticker) = recording_ticker("");
        let host = AutopilotHost::spawn(ticker, Duration::from_secs(60));

        host.set_targets(vec![target("a"), target("b")]);
        drain_until(&host, 2);
        thread::sleep(WAKE * 3);
        assert_eq!(
            *calls.lock().unwrap(),
            ["a", "b"],
            "no second tick inside the interval"
        );

        host.tick_now();
        drain_until(&host, 2);
        assert_eq!(*calls.lock().unwrap(), ["a", "b", "a", "b"]);
    }

    #[test]
    fn a_failing_tick_is_reported_and_the_loop_keeps_ticking() {
        let (calls, ticker) = recording_ticker("a");
        let host = AutopilotHost::spawn(ticker, Duration::from_millis(1));

        host.set_targets(vec![target("a"), target("b")]);
        let outcomes = drain_until(&host, 4);

        assert_eq!(outcomes[0].scope, "a");
        assert_eq!(outcomes[0].result, Err("gh: timed out".to_string()));
        assert_eq!(outcomes[1].scope, "b");
        assert!(outcomes[1].result.is_ok());
        assert!(
            calls.lock().unwrap().iter().filter(|s| *s == "a").count() >= 2,
            "the failing scope is ticked again"
        );
    }

    #[test]
    fn a_dropped_target_stops_being_ticked() {
        let (calls, ticker) = recording_ticker("");
        let host = AutopilotHost::spawn(ticker, Duration::from_millis(1));

        host.set_targets(vec![target("a")]);
        drain_until(&host, 1);
        host.set_targets(vec![target("b")]);
        // Let any tick of `a` already in flight land, then start counting.
        thread::sleep(WAKE * 2);
        host.drain();
        let before = calls.lock().unwrap().len();
        drain_until(&host, 2);

        let calls = calls.lock().unwrap();
        assert!(
            calls[before..].iter().all(|scope| scope == "b"),
            "{calls:?}"
        );
    }

    fn open_store(dir: &std::path::Path) -> RunStore {
        RunStore::open(&dir.join("runs.db")).unwrap()
    }

    #[test]
    fn status_reads_the_flag_and_the_newest_refusal() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        store.set_autopilot("github:o/r", true).unwrap();
        store
            .record_dispatch_event("github:o/r", None, REFUSED_EVENT, "paused: usage 87%")
            .unwrap();

        let status = load_status(
            &store,
            &["github:o/r".to_string(), "github:o/off".to_string()],
            2,
        )
        .unwrap();

        assert_eq!(
            status,
            [
                AutopilotStatus {
                    scope: "github:o/r".to_string(),
                    enabled: true,
                    running_lanes: 0,
                    max_lanes: 2,
                    last_refusal: Some("paused: usage 87%".to_string()),
                },
                AutopilotStatus {
                    scope: "github:o/off".to_string(),
                    enabled: false,
                    running_lanes: 0,
                    max_lanes: 2,
                    last_refusal: None,
                },
            ]
        );
    }

    #[test]
    fn a_launch_after_a_refusal_clears_it() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        store
            .record_dispatch_event("s", None, REFUSED_EVENT, "max lanes")
            .unwrap();
        store
            .record_dispatch_event("s", Some("GH-1"), LAUNCH_EVENT, "lane a")
            .unwrap();

        let status = load_status(&store, &["s".to_string()], 1).unwrap();

        assert_eq!(status[0].last_refusal, None);
    }

    #[test]
    fn summary_shows_lanes_and_the_refusal_or_tick_error() {
        let mut status = AutopilotStatus {
            scope: "s".to_string(),
            enabled: false,
            running_lanes: 1,
            max_lanes: 2,
            last_refusal: Some("paused: usage 87% ≥ 85%".to_string()),
        };
        assert_eq!(status.summary(None), "autopilot off");

        status.enabled = true;
        assert_eq!(
            status.summary(None),
            "autopilot on · lanes 1/2 · paused: usage 87% ≥ 85%"
        );
        assert_eq!(
            status.summary(Some("gh: timed out")),
            "autopilot on · lanes 1/2 · tick failed: gh: timed out"
        );

        status.last_refusal = None;
        assert_eq!(status.summary(None), "autopilot on · lanes 1/2");
    }
}
