//! The `tm init` / `tm update` agent setup session's run row (GitHub issue
//! #156).
//!
//! The setup session fills out the assets `tm init` and `tm update`
//! scaffold (see `crate::cli::init::offer_agent_setup`). Unlike every other
//! agent tm starts, it is neither tmux-hosted nor supervised: it runs as a
//! foreground child of the `tm` process, and the terminal belongs to it
//! until it exits. So there is no session to adopt a pre-registered row the
//! way an audit does. tm itself owns the row end to end:
//! [`run_setup_session`] starts it (`kind = "setup"`) before spawning,
//! stamps the child's pid once it exists, and finishes it from the child's
//! exit status.
//!
//! The row exists so memory-budget admission (GitHub issue #149) can see
//! the setup agent; it is telemetry, never a precondition. With no run
//! store, or a store that refuses a write, the session launches and runs
//! exactly as it would untracked.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use crate::agent::{AgentInvocation, TSKMSTR_RUN_ID, TSKMSTR_SESSION_RUN_ID};
use crate::runs::{FinishRun, RunStatus, RunStore, StartRun};

/// The setup session row's `kind`.
pub const SETUP_KIND: &str = "setup";

/// The setup session row's `lane`. A setup session belongs to no lane; the
/// kind doubles as the lane, as audit and bugbot-cleanup rows do.
pub const SETUP_LANE: &str = "setup";

/// The setup session row's `ticket`. A setup session works on no ticket, so
/// the field holds this fixed placeholder rather than a key.
pub const SETUP_TICKET: &str = "setup";

/// A spawned setup-session child, as [`run_setup_session`] needs it: a pid
/// to stamp on the row and an exit status to finish it from. A seam over
/// [`std::process::Child`] so tests can observe the row mid-session.
pub trait SetupChild {
    /// The child's process id.
    fn pid(&self) -> u32;

    /// Blocks until the child exits.
    ///
    /// # Errors
    ///
    /// Whatever the underlying wait fails with.
    fn wait(&mut self) -> io::Result<ExitStatus>;
}

impl SetupChild for std::process::Child {
    fn pid(&self) -> u32 {
        self.id()
    }

    fn wait(&mut self) -> io::Result<ExitStatus> {
        std::process::Child::wait(self)
    }
}

/// What [`run_setup_session`] records the session under.
pub struct SetupSession<'a> {
    /// Where to record the row. `None` when the run store could not be
    /// opened; the session then runs untracked.
    pub store: Option<&'a RunStore>,
    /// The row's scope: the repo's [`crate::config::BackendIdentity::scope`]
    /// when a config loads, else `""` (a first `tm init` may have no
    /// loadable config yet).
    pub scope: &'a str,
    /// [`crate::agent::AgentRunner::name`] of the runner the session
    /// launches, stamped as the row's `agent`.
    pub agent: &'a str,
    /// The directory the session runs in, when known (the repo the config
    /// lives in); `None` runs it in the current directory. Recorded as the
    /// row's worktree, and its name ([`crate::work::run::dir_repo_key`]) as
    /// the row's `repo`.
    pub dir: Option<&'a Path>,
    /// The agent program, for error messages.
    pub program: &'a str,
}

/// Runs the setup session in the foreground and records it as a
/// [`SETUP_KIND`] run row.
///
/// 1. Starts the row (`pid = None`, `agent`/`repo` stamped) before
///    spawning, so the row exists for the child's whole lifetime.
/// 2. Spawns the child through `spawn` and stamps its pid on the row, so
///    [`RunStore::reap`]'s liveness probe watches the agent rather than a
///    `NULL` pid going stale.
/// 3. Waits for it, then finishes the row [`RunStatus::Done`] on a
///    successful exit and [`RunStatus::Failed`] otherwise (with the exit
///    code when the child exited normally). A spawn or wait failure
///    finishes it failed too, so no row is left running.
///
/// Every store write is best-effort: a failed write leaves the row as it
/// was and the session carries on. A row a failed `finish_run` leaves
/// running is reaped once its stamped pid is gone.
///
/// # Errors
///
/// A message naming `session.program` if the child could not be spawned or
/// waited on, or exited unsuccessfully: the same messages the untracked
/// launcher reported.
pub fn run_setup_session<'c>(
    session: &SetupSession<'_>,
    spawn: &mut dyn FnMut() -> io::Result<Box<dyn SetupChild + 'c>>,
) -> Result<(), String> {
    let run_id = session
        .store
        .and_then(|store| start_setup_row(store, session));
    let finish = |status: RunStatus, exit_code: Option<i32>| {
        if let (Some(store), Some(run_id)) = (session.store, run_id) {
            let _ = store.finish_run(
                run_id,
                &FinishRun {
                    status,
                    exit_code,
                    ..FinishRun::default()
                },
            );
        }
    };

    let mut child = match spawn() {
        Ok(child) => child,
        Err(err) => {
            finish(RunStatus::Failed, None);
            return Err(format!("failed to launch {}: {err}", session.program));
        }
    };
    if let (Some(store), Some(run_id)) = (session.store, run_id) {
        let _ = store.update_pid(run_id, child.pid());
    }

    match child.wait() {
        Ok(status) if status.success() => {
            finish(RunStatus::Done, status.code());
            Ok(())
        }
        Ok(status) => {
            finish(RunStatus::Failed, status.code());
            Err(format!("{} exited with {status}", session.program))
        }
        Err(err) => {
            finish(RunStatus::Failed, None);
            Err(format!("failed to wait for {}: {err}", session.program))
        }
    }
}

/// Starts the setup row and stamps its estimate key, or `None` when the
/// store refuses the insert. A failed key stamp keeps the row: an
/// unestimable row still counts as a running agent.
fn start_setup_row(store: &RunStore, session: &SetupSession<'_>) -> Option<i64> {
    let dir: PathBuf = session
        .dir
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default();
    let run_id = store
        .start_run(&StartRun {
            ticket: SETUP_TICKET.to_string(),
            scope: session.scope.to_string(),
            lane: SETUP_LANE.to_string(),
            worktree: dir.to_string_lossy().into_owned(),
            branch: None,
            pid: None,
            kind: SETUP_KIND.to_string(),
            log_path: None,
        })
        .ok()?;
    let _ = store.update_agent_repo(run_id, session.agent, &crate::work::run::dir_repo_key(&dir));
    Some(run_id)
}

/// The [`Command`] that runs `invocation` as the setup session, in `dir`
/// when known.
///
/// Strips any inherited [`TSKMSTR_RUN_ID`] and [`TSKMSTR_SESSION_RUN_ID`]:
/// `tm init` or `tm update` run from inside a tracked agent session would
/// otherwise hand that session's run id to the setup agent, whose
/// telemetry hooks would then finish the *outer* session's row when the
/// setup session ends. The setup row is finished by
/// [`run_setup_session`], never by a hook, so the child gets neither
/// variable.
pub fn setup_command(invocation: &AgentInvocation, dir: Option<&Path>) -> Command {
    let mut command = Command::new(&invocation.program);
    command.args(&invocation.args);
    for var in &invocation.env_remove {
        command.env_remove(var);
    }
    command.env_remove(TSKMSTR_RUN_ID);
    command.env_remove(TSKMSTR_SESSION_RUN_ID);
    for (key, value) in &invocation.env_set {
        command.env(key, value);
    }
    if let Some(dir) = dir {
        command.current_dir(dir);
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runs::Run;
    use std::cell::RefCell;
    use std::os::unix::process::ExitStatusExt;
    use tempfile::TempDir;

    /// A child that exits with `raw` (a wait status: `code << 8`), and
    /// records the setup row as the store holds it while "running".
    struct FakeChild<'a> {
        pid: u32,
        raw: i32,
        store: &'a RunStore,
        seen: &'a RefCell<Option<Run>>,
    }

    impl SetupChild for FakeChild<'_> {
        fn pid(&self) -> u32 {
            self.pid
        }

        fn wait(&mut self) -> io::Result<ExitStatus> {
            if let Some(summary) = self.store.list_runs().unwrap().first() {
                *self.seen.borrow_mut() = self.store.run_by_id(summary.id).unwrap();
            }
            Ok(ExitStatus::from_raw(self.raw))
        }
    }

    fn session<'a>(store: Option<&'a RunStore>, dir: &'a Path) -> SetupSession<'a> {
        SetupSession {
            store,
            scope: "jira:proj",
            agent: "claude",
            dir: Some(dir),
            program: "claude",
        }
    }

    fn fixture() -> (TempDir, RunStore, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let repo = tmp.path().join("Projects/axiom");
        (tmp, store, repo)
    }

    fn only_run(store: &RunStore) -> Run {
        let runs = store.list_runs().unwrap();
        assert_eq!(runs.len(), 1, "exactly one setup row");
        store.run_by_id(runs[0].id).unwrap().unwrap()
    }

    #[test]
    fn the_row_is_running_with_the_child_pid_while_the_child_runs() {
        let (_tmp, store, repo) = fixture();
        let seen = RefCell::new(None);

        run_setup_session(&session(Some(&store), &repo), &mut || {
            Ok(Box::new(FakeChild {
                pid: 4242,
                raw: 0,
                store: &store,
                seen: &seen,
            }))
        })
        .unwrap();

        let running = seen.into_inner().expect("a row existed mid-session");
        assert_eq!(running.status, RunStatus::Running);
        assert_eq!(running.pid, Some(4242));
        assert_eq!(running.kind, SETUP_KIND);
        assert_eq!(running.lane, SETUP_LANE);
        assert_eq!(running.ticket, SETUP_TICKET);
        assert_eq!(running.scope, "jira:proj");
        assert_eq!(running.worktree, repo.to_string_lossy());
        assert_eq!(running.agent.as_deref(), Some("claude"));
        assert_eq!(running.repo.as_deref(), Some("axiom"));
    }

    #[test]
    fn a_successful_exit_finishes_the_row_done() {
        let (_tmp, store, repo) = fixture();
        let seen = RefCell::new(None);

        let result = run_setup_session(&session(Some(&store), &repo), &mut || {
            Ok(Box::new(FakeChild {
                pid: 4242,
                raw: 0,
                store: &store,
                seen: &seen,
            }))
        });

        assert_eq!(result, Ok(()));
        let run = only_run(&store);
        assert_eq!(run.status, RunStatus::Done);
        assert_eq!(run.exit_code, Some(0));
    }

    #[test]
    fn a_nonzero_exit_finishes_the_row_failed_with_its_code() {
        let (_tmp, store, repo) = fixture();
        let seen = RefCell::new(None);

        let result = run_setup_session(&session(Some(&store), &repo), &mut || {
            Ok(Box::new(FakeChild {
                pid: 4242,
                raw: 3 << 8,
                store: &store,
                seen: &seen,
            }))
        });

        let message = result.unwrap_err();
        assert!(message.starts_with("claude exited with"), "{message}");
        let run = only_run(&store);
        assert_eq!(run.status, RunStatus::Failed);
        assert_eq!(run.exit_code, Some(3));
    }

    #[test]
    fn a_spawn_failure_finishes_the_row_failed() {
        let (_tmp, store, repo) = fixture();

        let result = run_setup_session(&session(Some(&store), &repo), &mut || {
            Err(io::Error::new(io::ErrorKind::NotFound, "no such file"))
        });

        assert_eq!(
            result,
            Err("failed to launch claude: no such file".to_string())
        );
        let run = only_run(&store);
        assert_eq!(run.status, RunStatus::Failed);
        assert_eq!(run.pid, None);
    }

    #[test]
    fn without_a_store_the_session_still_runs() {
        let (_tmp, store, repo) = fixture();
        let seen = RefCell::new(None);
        let mut spawned = false;

        let result = run_setup_session(&session(None, &repo), &mut || {
            spawned = true;
            Ok(Box::new(FakeChild {
                pid: 4242,
                raw: 0,
                store: &store,
                seen: &seen,
            }))
        });

        assert_eq!(result, Ok(()));
        assert!(spawned);
        assert!(store.list_runs().unwrap().is_empty());
    }

    #[test]
    fn setup_command_strips_inherited_run_ids_and_runs_in_dir() {
        let invocation = AgentInvocation {
            program: "claude".to_string(),
            args: vec!["fill it out".to_string()],
            env_remove: vec!["ANTHROPIC_API_KEY".to_string()],
            env_set: Vec::new(),
        };

        let command = setup_command(&invocation, Some(Path::new("/r/axiom")));

        let removed: Vec<String> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect();
        assert!(removed.contains(&"ANTHROPIC_API_KEY".to_string()));
        assert!(removed.contains(&TSKMSTR_RUN_ID.to_string()));
        assert!(removed.contains(&TSKMSTR_SESSION_RUN_ID.to_string()));
        assert_eq!(command.get_current_dir(), Some(Path::new("/r/axiom")));
        assert_eq!(command.get_program(), "claude");
    }
}
