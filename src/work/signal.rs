//! Waiting on an interactive agent's done signal (GitHub issues #150 and
//! #184).
//!
//! An interactive agent sits at its prompt when it is done and never exits,
//! so a caller cannot wait for process exit. Instead the agent's prompt ends
//! with a completion section telling it to write a signal file, and the
//! caller polls for that file with [`wait_for_signal`], checking the step's
//! run row between polls ([`StepWatch::step_state`]). `tm review auto` waits
//! on its rounds and fix passes this way.
//!
//! Signal files are written under a temporary name ([`temporary_name`]) and
//! renamed into place, so a poll never reads a half-written file.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::runs::{Run, RunStatus};

/// Seconds between two checks for a step's signal file.
pub const SIGNAL_POLL_SECS: u64 = 10;

/// A step's run row, as a caller sees it while waiting for the step's
/// signal file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    /// Still running: keep waiting.
    Running,
    /// The idle sweep hibernated the agent before it signalled.
    Hibernated,
    /// Something other than the waiter ended the row (`tm merge` finishing
    /// the ticket's runs, the user exiting the session), or the ticket's
    /// session no longer exists: there is nothing left to wait on.
    Gone,
}

/// What [`wait_for_signal`] needs from its caller between polls.
pub trait StepWatch {
    /// The error [`StepWatch::step_state`] fails with.
    type Error;
    /// The state of the waited-on step's run row `run_id`.
    fn step_state(&self, run_id: i64) -> Result<StepState, Self::Error>;
    /// Waits `secs` seconds between two signal polls.
    fn sleep(&self, secs: u64);
    /// Seconds since the Unix epoch, for the signal timeout.
    fn now_unix_secs(&self) -> i64;
}

/// How waiting on one step's signal ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signal<T> {
    /// The signal file was read.
    Received(T),
    /// No signal before the timeout, or the agent was hibernated first.
    Stuck,
    /// See [`StepState::Gone`].
    Gone,
}

/// Polls `read` every [`SIGNAL_POLL_SECS`] until it yields the step's
/// signal, the step's row stops running, or `timeout_secs` pass. The signal
/// is read first on every poll, so a step that signalled before being
/// hibernated or ended still counts as done.
pub fn wait_for_signal<W, T>(
    watch: &W,
    run_id: i64,
    timeout_secs: i64,
    read: impl Fn() -> Option<T>,
) -> Result<Signal<T>, W::Error>
where
    W: StepWatch + ?Sized,
{
    let start = watch.now_unix_secs();
    loop {
        if let Some(signal) = read() {
            return Ok(Signal::Received(signal));
        }
        match watch.step_state(run_id)? {
            StepState::Running => {}
            StepState::Hibernated => return Ok(Signal::Stuck),
            StepState::Gone => return Ok(Signal::Gone),
        }
        if watch.now_unix_secs() - start >= timeout_secs {
            return Ok(Signal::Stuck);
        }
        watch.sleep(SIGNAL_POLL_SECS);
    }
}

/// The [`StepState`] of `run`, a step's row as just read from the run
/// store. A missing or ended row is [`StepState::Gone`], and so is a running
/// row whose `tmux_session` `session_alive` says no longer exists (`tm
/// merge` kills the ticket's session along with every window in it).
pub fn step_state_of<E>(
    run: Option<&Run>,
    session_alive: impl FnOnce(&str) -> Result<bool, E>,
) -> Result<StepState, E> {
    let Some(run) = run else {
        return Ok(StepState::Gone);
    };
    match run.status {
        RunStatus::Hibernated => return Ok(StepState::Hibernated),
        RunStatus::Running | RunStatus::Queued => {}
        _ => return Ok(StepState::Gone),
    }
    let alive = match run.tmux_session.as_deref() {
        Some(session) => session_alive(session)?,
        None => true,
    };
    Ok(if alive {
        StepState::Running
    } else {
        StepState::Gone
    })
}

/// The temporary name an agent writes a signal file under before renaming
/// it into place.
pub fn temporary_name(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    PathBuf::from(name)
}

#[derive(Deserialize)]
struct DoneMarker {
    succeeded: bool,
}

/// A done marker: `Some(succeeded)` once the file at `path` exists and
/// parses as `{"succeeded": <bool>}`, `None` before then.
pub fn read_done_marker(path: &Path) -> Option<bool> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<DoneMarker>(&raw)
        .ok()
        .map(|marker| marker.succeeded)
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::convert::Infallible;

    use tempfile::TempDir;

    use super::*;
    use crate::runs::{FinishRun, RunStore, StartRun};

    /// A [`StepWatch`] whose `step_state` replays `states` (the last one
    /// repeating) and whose `sleep` advances its clock.
    struct FakeWatch {
        states: RefCell<Vec<StepState>>,
        clock: Cell<i64>,
        sleeps: Cell<u32>,
    }

    impl FakeWatch {
        fn new(states: Vec<StepState>) -> Self {
            FakeWatch {
                states: RefCell::new(states),
                clock: Cell::new(1_000),
                sleeps: Cell::new(0),
            }
        }
    }

    impl StepWatch for FakeWatch {
        type Error = Infallible;

        fn step_state(&self, _run_id: i64) -> Result<StepState, Infallible> {
            let mut states = self.states.borrow_mut();
            Ok(if states.len() > 1 {
                states.remove(0)
            } else {
                states[0]
            })
        }

        fn sleep(&self, secs: u64) {
            self.clock.set(self.clock.get() + secs as i64);
            self.sleeps.set(self.sleeps.get() + 1);
        }

        fn now_unix_secs(&self) -> i64 {
            self.clock.get()
        }
    }

    #[test]
    fn a_signal_read_on_the_first_poll_counts_even_when_the_row_is_gone() {
        let watch = FakeWatch::new(vec![StepState::Gone]);
        let got = wait_for_signal(&watch, 1, 60, || Some(7)).unwrap();
        assert_eq!(got, Signal::Received(7));
        assert_eq!(watch.sleeps.get(), 0);
    }

    #[test]
    fn a_row_that_ends_without_a_signal_is_gone() {
        let watch = FakeWatch::new(vec![StepState::Running, StepState::Gone]);
        let got = wait_for_signal(&watch, 1, 600, || None::<()>).unwrap();
        assert_eq!(got, Signal::Gone);
        assert_eq!(watch.sleeps.get(), 1);
    }

    #[test]
    fn a_hibernated_row_without_a_signal_is_stuck() {
        let watch = FakeWatch::new(vec![StepState::Hibernated]);
        let got = wait_for_signal(&watch, 1, 600, || None::<()>).unwrap();
        assert_eq!(got, Signal::Stuck);
    }

    #[test]
    fn a_running_row_that_never_signals_is_stuck_once_the_timeout_passes() {
        let watch = FakeWatch::new(vec![StepState::Running]);
        let got = wait_for_signal(&watch, 1, 30, || None::<()>).unwrap();
        assert_eq!(got, Signal::Stuck);
        assert_eq!(watch.sleeps.get(), 3);
        assert!(watch.now_unix_secs() - 1_000 >= 30);
    }

    #[test]
    fn read_done_marker_is_none_until_the_marker_parses() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("step.done");
        assert_eq!(read_done_marker(&path), None, "missing");

        std::fs::write(&path, "{\"succ").unwrap();
        assert_eq!(read_done_marker(&path), None, "half-written");

        std::fs::write(&path, r#"{"succeeded": false}"#).unwrap();
        assert_eq!(read_done_marker(&path), Some(false));
    }

    #[test]
    fn temporary_name_appends_tmp() {
        assert_eq!(
            temporary_name(Path::new("/g/fix-1.done")),
            PathBuf::from("/g/fix-1.done.tmp")
        );
    }

    fn store(dir: &Path) -> RunStore {
        RunStore::open(&dir.join("runs.db")).unwrap()
    }

    fn start(store: &RunStore) -> i64 {
        store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                scope: String::new(),
                lane: "backend".to_string(),
                worktree: "/wt/proj-1".to_string(),
                branch: None,
                pid: None,
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap()
    }

    fn state_of(store: &RunStore, run_id: i64, alive: bool) -> StepState {
        let run = store.run_by_id(run_id).unwrap();
        step_state_of(run.as_ref(), |_| Ok::<_, Infallible>(alive)).unwrap()
    }

    #[test]
    fn step_state_of_a_missing_row_is_gone() {
        assert_eq!(
            step_state_of(None, |_| Ok::<_, Infallible>(true)).unwrap(),
            StepState::Gone
        );
    }

    #[test]
    fn step_state_of_a_done_row_is_gone() {
        let tmp = TempDir::new().unwrap();
        let store = store(tmp.path());
        let id = start(&store);
        store
            .finish_run(
                id,
                &FinishRun {
                    status: RunStatus::Done,
                    ..FinishRun::default()
                },
            )
            .unwrap();
        assert_eq!(state_of(&store, id, true), StepState::Gone);
    }

    #[test]
    fn step_state_of_a_hibernated_row_is_hibernated() {
        let tmp = TempDir::new().unwrap();
        let store = store(tmp.path());
        let id = start(&store);
        assert!(store.hibernate_run(id).unwrap());
        assert_eq!(state_of(&store, id, true), StepState::Hibernated);
    }

    #[test]
    fn step_state_of_a_running_row_follows_its_session() {
        let tmp = TempDir::new().unwrap();
        let store = store(tmp.path());
        let id = start(&store);
        assert_eq!(
            state_of(&store, id, false),
            StepState::Running,
            "no tmux_session: session_alive is never asked"
        );

        store.update_tmux_session(id, "tm-proj-1").unwrap();
        assert_eq!(state_of(&store, id, false), StepState::Gone);
        assert_eq!(state_of(&store, id, true), StepState::Running);
    }
}
