//! Child-process seam for board-launched lane runs (`w` on the Jira board),
//! per `docs/plans/board-lane-runs.md`'s "Launch mechanism" decision.
//!
//! [`LaneLauncher::spawn`] starts a short-lived *launcher* child -- `tm work
//! run <lane> <key>` via `std::env::current_exe()` -- with piped
//! stdout/stderr. `tm work run` (no `--fg`) deliberately does all
//! provisioning/preflight in the foreground and creates no run row until it
//! succeeds (see `crate::work::run::prepare_run_lane`'s doc comment), then
//! re-execs and detaches the actual supervisor (see
//! `crate::work::detach::RealDetachSpawner`) and exits within seconds -- so
//! this watched child resolves quickly either way: exit 0 means the run row
//! exists and badge polling (`Cmd::LoadLaneRunStatus`) takes over; a nonzero
//! exit means preflight failed and the launcher's stderr has the reason.
//!
//! [`LaunchHandle::try_finish`] polls non-blockingly (`Child::try_wait`) so
//! the single-threaded TUI event loop never blocks on a launch -- see
//! `crate::tui::event::run`'s per-iteration registry poll.
//!
//! [`RealLaneLauncher`]/[`RealLaunchHandle`] are the production
//! implementation; [`FakeLaneLauncher`]/[`FakeLaunchHandle`] are test
//! doubles, following the trait+fake seam used throughout `src/work/`
//! ([`crate::work::tmux::TmuxOps`], [`crate::work::detach::DetachSpawner`]).

use std::cell::RefCell;
use std::io::Read;
use std::process::{Child, Command, Stdio};

/// Maximum length (bytes) of the stderr snippet surfaced on a nonzero exit,
/// per `docs/plans/board-lane-runs.md`.
const STDERR_SNIPPET_LIMIT: usize = 200;

/// Spawns the launcher child for a board-triggered lane run.
pub trait LaneLauncher {
    /// Spawn `tm work run <lane> <key>` with piped stdout/stderr, returning a
    /// handle to poll for completion. An `Err` here means the launcher
    /// process itself could not be spawned at all (e.g. `current_exe()`
    /// failed) -- distinct from the *launched* process later exiting
    /// nonzero, which [`LaunchHandle::try_finish`] reports instead.
    fn spawn(&self, lane: &str, key: &str) -> Result<Box<dyn LaunchHandle>, String>;
}

/// A launcher child in flight, polled non-blockingly for completion.
pub trait LaunchHandle {
    /// Poll without blocking. `None` while still running; `Some(Ok(()))` on
    /// a zero exit; `Some(Err(message))` on a nonzero exit (or a `try_wait`
    /// I/O error), `message` being the launcher's captured stderr (first
    /// non-empty line, truncated to ~200 bytes) when one is available.
    fn try_finish(&mut self) -> Option<Result<(), String>>;
}

/// Production [`LaneLauncher`]: spawns `std::env::current_exe() work run
/// <lane> <key>`.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealLaneLauncher;

impl LaneLauncher for RealLaneLauncher {
    fn spawn(&self, lane: &str, key: &str) -> Result<Box<dyn LaunchHandle>, String> {
        let program = std::env::current_exe().map_err(|err| err.to_string())?;
        let child = Command::new(program)
            .args(["work", "run", lane, key])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| err.to_string())?;
        Ok(Box::new(RealLaunchHandle { child }))
    }
}

/// Production [`LaunchHandle`]: wraps a real [`Child`], reading its captured
/// stderr only once it has exited nonzero (reading a live pipe up front
/// would risk blocking on a still-running process, which `try_finish`'s
/// non-blocking contract forbids).
///
/// ## What's unit-tested vs. deferred to manual verification
///
/// [`try_finish`](LaunchHandle::try_finish)'s polling/exit-code/stderr-capture
/// logic is exercised directly against real short-lived `sh -c` children in
/// this module's tests (constructing [`RealLaunchHandle`] around them,
/// bypassing [`RealLaneLauncher::spawn`]'s `current_exe()` argv). The actual
/// `tm work run <lane> <key>` argv and its interaction with a real board
/// session are not unit-tested -- see `crate::work::detach`'s "What's
/// unit-tested vs. deferred" section for why that class of mechanics isn't;
/// verify manually per `docs/plans/board-lane-runs.md`: press `w` on a
/// ticket with a configured lane, confirm the badge reads `Starting` then
/// `Running`/`Waiting`/`Done`, and confirm an unconfigured/unknown lane
/// surfaces its preflight error text in the status line.
struct RealLaunchHandle {
    child: Child,
}

impl LaunchHandle for RealLaunchHandle {
    fn try_finish(&mut self) -> Option<Result<(), String>> {
        match self.child.try_wait() {
            Ok(None) => None,
            Ok(Some(status)) if status.success() => Some(Ok(())),
            Ok(Some(status)) => {
                let stderr = self
                    .child
                    .stderr
                    .take()
                    .map(|mut pipe| {
                        let mut buf = String::new();
                        let _ = pipe.read_to_string(&mut buf);
                        buf
                    })
                    .unwrap_or_default();
                Some(Err(stderr_snippet(&stderr, status)))
            }
            Err(err) => Some(Err(err.to_string())),
        }
    }
}

/// Reduces captured stderr to its first non-empty line, truncated to
/// [`STDERR_SNIPPET_LIMIT`] bytes, falling back to the exit status itself
/// when stderr was empty or unavailable.
fn stderr_snippet(stderr: &str, status: std::process::ExitStatus) -> String {
    match stderr.lines().find(|line| !line.trim().is_empty()) {
        Some(line) => truncate(line.trim(), STDERR_SNIPPET_LIMIT),
        None => format!("exited with {status}"),
    }
}

/// Truncates `s` to at most `max` bytes, on a `char` boundary, appending
/// `...` when truncated.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

/// Test double for [`LaneLauncher`]: returns a scripted spawn outcome
/// instead of spawning a real process. On success, the returned
/// [`FakeLaunchHandle`] replays `finish_sequence` (see
/// [`FakeLaneLauncher::with_finish_sequence`]) across successive
/// `try_finish` calls, repeating its last entry once exhausted.
pub struct FakeLaneLauncher {
    spawn_result: RefCell<Result<(), String>>,
    finish_sequence: Vec<Option<Result<(), String>>>,
    calls: RefCell<Vec<(String, String)>>,
}

impl FakeLaneLauncher {
    /// A fake whose `spawn` succeeds and whose handle finishes `Ok(())` on
    /// the very first `try_finish` call, unless overridden.
    pub fn new() -> Self {
        Self {
            spawn_result: RefCell::new(Ok(())),
            finish_sequence: vec![Some(Ok(()))],
            calls: RefCell::new(Vec::new()),
        }
    }

    /// Make every `spawn` call fail with `message`.
    pub fn with_spawn_error(self, message: impl Into<String>) -> Self {
        *self.spawn_result.borrow_mut() = Err(message.into());
        self
    }

    /// Script the sequence of `try_finish` outcomes a spawned handle
    /// replays, e.g. `vec![None, None, Some(Ok(()))]` to simulate two
    /// still-running polls before completion.
    pub fn with_finish_sequence(mut self, sequence: Vec<Option<Result<(), String>>>) -> Self {
        self.finish_sequence = sequence;
        self
    }

    /// Every `(lane, key)` pair passed to `spawn`, in call order.
    pub fn calls(&self) -> Vec<(String, String)> {
        self.calls.borrow().clone()
    }
}

impl Default for FakeLaneLauncher {
    fn default() -> Self {
        Self::new()
    }
}

impl LaneLauncher for FakeLaneLauncher {
    fn spawn(&self, lane: &str, key: &str) -> Result<Box<dyn LaunchHandle>, String> {
        self.calls
            .borrow_mut()
            .push((lane.to_string(), key.to_string()));
        self.spawn_result.borrow().clone()?;
        Ok(Box::new(FakeLaunchHandle {
            sequence: self.finish_sequence.clone(),
            index: 0,
        }))
    }
}

/// Test double for [`LaunchHandle`]: replays a scripted sequence of
/// `try_finish` outcomes.
struct FakeLaunchHandle {
    sequence: Vec<Option<Result<(), String>>>,
    index: usize,
}

impl LaunchHandle for FakeLaunchHandle {
    fn try_finish(&mut self) -> Option<Result<(), String>> {
        let outcome = self
            .sequence
            .get(self.index)
            .or_else(|| self.sequence.last())
            .cloned()
            .unwrap_or(None);
        if self.index < self.sequence.len() {
            self.index += 1;
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn sh_handle(script: &str) -> RealLaunchHandle {
        let child = Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("sh should spawn");
        RealLaunchHandle { child }
    }

    /// Polls `handle` until it reports completion, sleeping briefly between
    /// polls (mirrors production usage, where the event loop polls once per
    /// iteration rather than blocking).
    fn wait_for_finish(mut handle: RealLaunchHandle) -> Result<(), String> {
        loop {
            if let Some(result) = handle.try_finish() {
                return result;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn try_finish_on_zero_exit_is_ok() {
        let handle = sh_handle("exit 0");
        assert_eq!(wait_for_finish(handle), Ok(()));
    }

    #[test]
    fn try_finish_on_nonzero_exit_surfaces_stderr_first_line() {
        let handle = sh_handle("echo boom 1>&2; echo more 1>&2; exit 1");
        assert_eq!(wait_for_finish(handle), Err("boom".to_string()));
    }

    #[test]
    fn try_finish_on_nonzero_exit_with_no_stderr_falls_back_to_exit_status() {
        let handle = sh_handle("exit 7");
        let err = wait_for_finish(handle).expect_err("expected an error");
        assert!(err.contains("exit status: 7") || err.contains('7'));
    }

    #[test]
    fn try_finish_truncates_long_stderr() {
        let long = "x".repeat(400);
        let handle = sh_handle(&format!("echo {long} 1>&2; exit 1"));
        let err = wait_for_finish(handle).expect_err("expected an error");
        assert!(err.len() <= STDERR_SNIPPET_LIMIT + 3);
        assert!(err.ends_with("..."));
    }

    // --- FakeLaneLauncher / FakeLaunchHandle ---

    #[test]
    fn fake_spawn_records_lane_and_key() {
        let fake = FakeLaneLauncher::new();
        let _ = fake.spawn("backend", "PROJ-1");
        assert_eq!(
            fake.calls(),
            vec![("backend".to_string(), "PROJ-1".to_string())]
        );
    }

    #[test]
    fn fake_spawn_error_surfaces_as_err() {
        let fake = FakeLaneLauncher::new().with_spawn_error("boom");
        let result = fake.spawn("backend", "PROJ-1");
        assert_eq!(result.err(), Some("boom".to_string()));
    }

    #[test]
    fn fake_handle_replays_scripted_finish_sequence() {
        let fake = FakeLaneLauncher::new().with_finish_sequence(vec![None, None, Some(Ok(()))]);
        let mut handle = fake
            .spawn("backend", "PROJ-1")
            .expect("spawn should succeed");
        assert_eq!(handle.try_finish(), None);
        assert_eq!(handle.try_finish(), None);
        assert_eq!(handle.try_finish(), Some(Ok(())));
    }

    #[test]
    fn fake_handle_repeats_last_entry_once_exhausted() {
        let fake =
            FakeLaneLauncher::new().with_finish_sequence(vec![Some(Err("boom".to_string()))]);
        let mut handle = fake
            .spawn("backend", "PROJ-1")
            .expect("spawn should succeed");
        assert_eq!(handle.try_finish(), Some(Err("boom".to_string())));
        assert_eq!(handle.try_finish(), Some(Err("boom".to_string())));
    }
}
