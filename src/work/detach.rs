//! Detachment mechanics for `tm work run --headless`, per
//! `docs/plans/runner-port.md` step 10 — the step the plan's Risks section
//! flags as the hardest, because Rust doesn't get detachment for free the
//! way `work.ml` did.
//!
//! # Why this exists (read this before touching [`RealDetachSpawner`])
//!
//! `work.ml`'s detached mode is `nohup sh '<wrapper>' >>log 2>&1 </dev/null
//! &` via `Sys.command`: shell job control handles detachment, and the
//! *wrapper script* — not `j` itself — does the waiting and calls `tm runs
//! finish`, so `j` returns the terminal immediately without needing to
//! survive its own child.
//!
//! `docs/plans/runner-port.md` §4 deliberately moves that wait-then-finish
//! logic in-process (one Rust function, [`crate::work::run::run_claude_and_finish`],
//! shared by `--fg` and detached instead of duplicated between `--fg`'s
//! inline `jq` calls and a generated shell wrapper's `jq` calls). That
//! architecture win has a cost: something now has to *stay alive* after
//! `tm work run` returns the terminal, to eventually call `RunStore::finish_run`
//! when `claude` exits. The plan's chosen answer, implemented here:
//!
//! 1. `tm work run <lane> --headless` does all provisioning, preflight, and
//!    `RunStore::start_run` in the *foreground* — errors surface to the
//!    user immediately, and no run row is created until it's known the run
//!    is actually going to happen (see
//!    [`crate::work::run::prepare_run_lane`]).
//! 2. It writes everything the spawn-wait-finish tail needs
//!    ([`crate::work::run::PreparedRun`]) to a JSON state file.
//! 3. It re-execs *itself* (`std::env::current_exe()`) with a hidden
//!    subcommand ([`supervisor_argv`]) pointing at that state file, detached
//!    via `setsid` ([`RealDetachSpawner`]) with stdio redirected to the
//!    run's log file, then exits.
//! 4. The re-exec'd child — `tm work __supervise` — deserializes the state
//!    file and calls [`crate::work::run::supervise_run`]: record its own pid
//!    (see that function's doc comment for why this two-step pid handoff
//!    matters for `tm runs reap`), then spawn `claude`, wait, parse, and
//!    `RunStore::finish_run`. This is the *same* function `--fg` calls for
//!    its own tail — no duplicated parsing logic, per §4.
//!
//! # Deviation from the plan's suggested subcommand surface
//!
//! The plan sketches `tm work __supervise --run-id N --outcome-json <path>
//! ...` — one flag per field. This implementation instead passes a single
//! `--state-file <path>` pointing at a JSON-serialized
//! [`crate::work::run::PreparedRun`] (see [`supervisor_argv`]). Reasons:
//! the prompt text alone can be arbitrarily long and contain arbitrary
//! bytes, and re-flattening the full invocation (argv, env, paths) into a
//! flag-per-field surface either re-introduces shell-quoting-shaped bugs or
//! requires a repeat-until-exhausted flag design (`--arg`, `--env-set`,
//! ...) that's harder to keep in sync with `PreparedRun`'s fields as they
//! change. A single opaque state file is simpler, keeps the argv-building
//! code trivially pure ([`supervisor_argv`]) and testable, and there is no
//! `std::process::Command` shell in between to worry about quoting for.
//!
//! # Why `setsid`, not `process_group(0)`
//!
//! `std::os::unix::process::CommandExt::process_group` (stable since Rust
//! 1.64) only detaches the child into a new *process group*; it does not
//! call `setsid(2)`, so the child keeps the parent's controlling terminal
//! and session. `libc` is already a dependency of this crate (see
//! `src/work/run.rs`'s `SystemClock`), so [`RealDetachSpawner`] instead runs
//! `libc::setsid()` in a `pre_exec` hook — the same primitive `work.ml`'s
//! shell-level detachment relied on (`nohup ... &` detaches the job from
//! the shell's job control, and a background job in a script run under
//! `sh -c` typically ends up session-leaderless in a comparable way) — so
//! the child becomes its own session leader with no controlling terminal at
//! all, immune to the parent terminal's `SIGHUP` on close.
//!
//! # Crash-safety / zombie-avoidance argument
//!
//! - **Row stuck `running` forever**: guarded by [`RunStore::reap`]
//!   (`crate::runs`), which now probes the pid the *supervisor* recorded via
//!   `update_pid`, not the short-lived parent's pid — see
//!   [`crate::work::run::prepare_run_lane`]'s doc comment.
//! - **Zombie processes**: the supervisor is the one that calls
//!   `Command::spawn` + `.wait()` on `claude` (via
//!   [`crate::work::runner::ProcessSpawner`]), so that grandchild is always
//!   reaped by its immediate parent. The supervisor itself is `setsid`'d and
//!   detached from `tm work run`, which never calls `.wait()` on it and
//!   exits immediately after spawning — once `tm work run` exits, the
//!   supervisor (if it also happens to exit around the same time) gets
//!   reparented to the OS's init process, which reaps orphans. There is a
//!   theoretically nonzero but practically negligible window between spawn
//!   and the parent's exit; no double-fork is used because `tm work run`'s
//!   own lifetime after spawning is a handful of `writeln!` calls, not a
//!   long-lived process that could itself become the zombie's un-reaping
//!   parent.
//! - **Double supervision**: each `tm work run` invocation creates exactly
//!   one run row and re-execs exactly one supervisor for it; nothing here
//!   retries or re-spawns.
//!
//! # What's unit-tested vs. deferred to manual verification
//!
//! [`supervisor_argv`] (pure) and [`FakeDetachSpawner`] (records what it was
//! asked to do) are unit-tested. The actual `setsid` + stdio-redirection +
//! self-re-exec mechanics in [`RealDetachSpawner`] cannot be meaningfully
//! unit-tested (they spawn a real detached process and outlive the test's
//! own process tree) — see the manual test plan below, to run during the
//! E2E dogfood step of `docs/plans/runner-port.md`.
//!
//! ## Manual test plan
//!
//! 1. Configure a lane pointing at a small/fast throwaway repo.
//! 2. Run `tm work run <lane> --headless --max-turns 1` from an interactive
//!    shell.
//! 3. Confirm the terminal returns immediately with the `started`/`log`/
//!    `follow`/`watch` summary (see [`crate::cli::work::run`]).
//! 4. Close the terminal (or `kill` the shell) before `claude` finishes.
//! 5. Confirm via `ps` that the supervisor process (`tm work __supervise
//!    ...`) is still running, has no controlling terminal
//!    (`ps -o tty,pid,ppid,command`), and is not a zombie.
//! 6. Wait for the run to finish; confirm `tm runs show <ticket-or-lane>`
//!    reaches `done`/`failed` and `tail -f <log>` shows the printed summary.
//! 7. Repeat step 4 but `kill -9` the supervisor process mid-run instead of
//!    letting it finish; confirm `tm runs reap` (after `stale_after_mins`
//!    elapses) marks the row `failed` rather than leaving it `running`
//!    forever.

use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::work::run::PreparedRun;

/// The full contents of the JSON state file `tm work run` (detached) writes
/// for its re-exec'd supervisor: [`PreparedRun`] (everything
/// [`crate::work::run::supervise_run`] needs to spawn `claude` and finish
/// the tracked run) plus the run database path, since the supervisor is a
/// separate process that shares no memory with its parent and so can't
/// otherwise know which `RunStore` the parent resolved.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SupervisorState {
    /// Everything the spawn-wait-parse-finish tail needs.
    pub prepared: PreparedRun,
    /// The run database path to open a [`crate::runs::RunStore`] against.
    pub run_db_path: PathBuf,
}

/// Errors from [`DetachSpawner::spawn_detached`].
#[derive(Debug, Error)]
pub enum DetachError {
    /// The re-exec'd supervisor process could not be spawned at all.
    #[error("failed to spawn detached supervisor `{program}`: {message}")]
    Spawn {
        /// The program that could not be spawned (`current_exe`).
        program: String,
        /// The underlying I/O error message.
        message: String,
    },

    /// The log file the supervisor's stdio should redirect to could not be
    /// opened/created.
    #[error("failed to open log file {path}: {message}")]
    LogFile {
        /// The log file path that could not be opened.
        path: PathBuf,
        /// The underlying I/O error message.
        message: String,
    },
}

/// Builds the argv for the hidden `tm work __supervise` subcommand, given
/// the path to a JSON-serialized [`crate::work::run::PreparedRun`] state
/// file. Pure — no process spawning, no file I/O — so it's testable without
/// any of [`RealDetachSpawner`]'s OS-level mechanics.
///
/// Excludes the program name itself (`current_exe()`'s path): the caller
/// passes this alongside the program to whichever [`DetachSpawner`] it uses.
pub fn supervisor_argv(state_file: &Path) -> Vec<String> {
    vec![
        "work".to_string(),
        "__supervise".to_string(),
        "--state-file".to_string(),
        state_file.to_string_lossy().into_owned(),
    ]
}

/// Spawns a detached supervisor process. [`RealDetachSpawner`] is the
/// production implementation (re-exec + `setsid` + stdio redirection);
/// [`FakeDetachSpawner`] records the call for tests instead of spawning
/// anything, mirroring the trait+fake seam used throughout `src/work/`
/// ([`crate::work::git::GitOps`], [`crate::work::runner::ProcessSpawner`],
/// ...).
pub trait DetachSpawner {
    /// Spawn `program argv...` fully detached: a new session (no
    /// controlling terminal), stdin from `/dev/null`, stdout/stderr
    /// appended to `log_path`, current directory `working_dir`. Returns the
    /// spawned process's pid. Does not wait for it — by design, the whole
    /// point is that the caller returns without waiting.
    fn spawn_detached(
        &self,
        program: &Path,
        argv: &[String],
        working_dir: &Path,
        log_path: &Path,
    ) -> Result<u32, DetachError>;
}

/// Production [`DetachSpawner`]: re-execs `program` (in practice,
/// `std::env::current_exe()`, i.e. `tm` itself) with `argv`, `setsid`'d into
/// its own session via a `pre_exec` hook, stdin from `/dev/null`,
/// stdout/stderr appended to `log_path`. See this module's doc comment for
/// why `setsid` rather than `process_group(0)`.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealDetachSpawner;

impl DetachSpawner for RealDetachSpawner {
    #[cfg(unix)]
    fn spawn_detached(
        &self,
        program: &Path,
        argv: &[String],
        working_dir: &Path,
        log_path: &Path,
    ) -> Result<u32, DetachError> {
        use std::fs::OpenOptions;
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};

        let log_out = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .map_err(|err| DetachError::LogFile {
                path: log_path.to_path_buf(),
                message: err.to_string(),
            })?;
        let log_err = log_out.try_clone().map_err(|err| DetachError::LogFile {
            path: log_path.to_path_buf(),
            message: err.to_string(),
        })?;

        let mut command = Command::new(program);
        command
            .args(argv)
            .current_dir(working_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_out))
            .stderr(Stdio::from(log_err));

        // SAFETY: `libc::setsid()` is async-signal-safe and is the only
        // thing this hook does between fork and exec; it takes no
        // arguments and its only effect is on the calling (post-fork
        // child) process's session membership, so it cannot race or
        // corrupt parent state. Detaching from the controlling terminal
        // this way — not `process_group(0)` — is exactly what makes the
        // supervisor immune to the parent terminal's `SIGHUP` on close;
        // see this module's doc comment.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let child = command.spawn().map_err(|err| DetachError::Spawn {
            program: program.to_string_lossy().into_owned(),
            message: err.to_string(),
        })?;

        Ok(child.id())
    }

    #[cfg(not(unix))]
    fn spawn_detached(
        &self,
        program: &Path,
        argv: &[String],
        working_dir: &Path,
        log_path: &Path,
    ) -> Result<u32, DetachError> {
        // No setsid/process-group primitive on non-unix targets; this
        // codebase's tmux/git-worktree lane runner is unix-only in
        // practice, so this branch exists only so the crate builds
        // elsewhere, not as a supported detachment path.
        use std::fs::OpenOptions;
        use std::process::{Command, Stdio};

        let log_out = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .map_err(|err| DetachError::LogFile {
                path: log_path.to_path_buf(),
                message: err.to_string(),
            })?;
        let log_err = log_out.try_clone().map_err(|err| DetachError::LogFile {
            path: log_path.to_path_buf(),
            message: err.to_string(),
        })?;

        let child = Command::new(program)
            .args(argv)
            .current_dir(working_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_out))
            .stderr(Stdio::from(log_err))
            .spawn()
            .map_err(|err| DetachError::Spawn {
                program: program.to_string_lossy().into_owned(),
                message: err.to_string(),
            })?;

        Ok(child.id())
    }
}

/// A recorded [`DetachSpawner::spawn_detached`] call, for test assertions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedDetach {
    /// The program that would have been re-exec'd.
    pub program: PathBuf,
    /// The argv it would have been given.
    pub argv: Vec<String>,
    /// The working directory it would have been spawned in.
    pub working_dir: PathBuf,
    /// The log file its stdio would have been redirected to.
    pub log_path: PathBuf,
}

/// Test double for [`DetachSpawner`]: records the call it was given instead
/// of spawning anything, and returns a canned pid.
pub struct FakeDetachSpawner {
    /// The pid to report back.
    pub pid: u32,
    /// Every call made, in order.
    pub recorded: std::sync::Mutex<Vec<RecordedDetach>>,
}

impl FakeDetachSpawner {
    /// A fake that reports `pid` on every call.
    pub fn new(pid: u32) -> Self {
        Self {
            pid,
            recorded: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl DetachSpawner for FakeDetachSpawner {
    fn spawn_detached(
        &self,
        program: &Path,
        argv: &[String],
        working_dir: &Path,
        log_path: &Path,
    ) -> Result<u32, DetachError> {
        self.recorded
            .lock()
            .expect("FakeDetachSpawner mutex poisoned")
            .push(RecordedDetach {
                program: program.to_path_buf(),
                argv: argv.to_vec(),
                working_dir: working_dir.to_path_buf(),
                log_path: log_path.to_path_buf(),
            });
        Ok(self.pid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentInvocation;

    fn sample_prepared_run() -> PreparedRun {
        PreparedRun {
            run_id: 1,
            lane: "mylane".to_string(),
            ticket: Some("ABC-123".to_string()),
            wt_name: "abc-123".to_string(),
            timestamp: "20260101-120000".to_string(),
            worktree: PathBuf::from("/Worktrees/axiom/abc-123"),
            branch: "claude/abc-123-20260101-120000".to_string(),
            invocation: AgentInvocation {
                program: "claude".to_string(),
                args: vec!["-p".to_string(), "do the thing".to_string()],
                env_set: vec![("TSKMSTR_RUN_ID".to_string(), "1".to_string())],
                env_remove: vec!["ANTHROPIC_API_KEY".to_string()],
            },
            out_json_path: PathBuf::from("/state/abc-123-20260101-120000.json"),
        }
    }

    #[test]
    fn supervisor_state_round_trips_through_json() {
        let state = SupervisorState {
            prepared: sample_prepared_run(),
            run_db_path: PathBuf::from("/Users/jowi/.local/state/tskmstr/runs.db"),
        };

        let json = serde_json::to_string(&state).unwrap();
        let round_tripped: SupervisorState = serde_json::from_str(&json).unwrap();

        assert_eq!(round_tripped.run_db_path, state.run_db_path);
        assert_eq!(round_tripped.prepared.run_id, state.prepared.run_id);
        assert_eq!(round_tripped.prepared.branch, state.prepared.branch);
        assert_eq!(round_tripped.prepared.invocation, state.prepared.invocation);
    }

    #[test]
    fn supervisor_argv_points_at_the_hidden_subcommand_and_state_file() {
        let argv = supervisor_argv(Path::new("/tmp/lane-20260101.supervisor.json"));

        assert_eq!(
            argv,
            vec![
                "work".to_string(),
                "__supervise".to_string(),
                "--state-file".to_string(),
                "/tmp/lane-20260101.supervisor.json".to_string(),
            ]
        );
    }

    #[test]
    fn supervisor_argv_preserves_paths_with_spaces_verbatim() {
        // No shell sits between this argv and exec(), so a path with a
        // space needs no quoting/escaping — it's a single argv element.
        let argv = supervisor_argv(Path::new("/tmp/my lane-20260101.supervisor.json"));

        assert_eq!(argv[3], "/tmp/my lane-20260101.supervisor.json");
    }

    #[test]
    fn fake_detach_spawner_records_the_call_and_returns_its_canned_pid() {
        let spawner = FakeDetachSpawner::new(4242);

        let pid = spawner
            .spawn_detached(
                Path::new("/usr/local/bin/tm"),
                &["work".to_string(), "__supervise".to_string()],
                Path::new("/Worktrees/axiom/mylane"),
                Path::new("/state/mylane-20260101.log"),
            )
            .unwrap();

        assert_eq!(pid, 4242);
        let recorded = spawner.recorded.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].program, PathBuf::from("/usr/local/bin/tm"));
        assert_eq!(
            recorded[0].argv,
            vec!["work".to_string(), "__supervise".to_string()]
        );
        assert_eq!(
            recorded[0].working_dir,
            PathBuf::from("/Worktrees/axiom/mylane")
        );
        assert_eq!(
            recorded[0].log_path,
            PathBuf::from("/state/mylane-20260101.log")
        );
    }
}
