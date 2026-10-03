//! Hibernating idle interactive runs, and waking them on attach (GitHub
//! issue #64).
//!
//! An interactive run's agent process lives in a tmux window and, left
//! alone, sits at its prompt forever after the agent stops working — each
//! one holding hundreds of MB. [`sweep_idle`] stops agents that have been
//! quiet longer than `[work] idle_hibernate_mins`, and [`wake_session`]
//! brings them back, in the same conversation, when the user attaches to
//! the ticket's session.
//!
//! # What makes a run hibernatable
//!
//! All of, checked cheapest-first:
//!
//! - `running`, tmux-hosted, with a recorded `session_id`, and a heartbeat
//!   older than the threshold ([`RunStore::idle_interactive_runs`]). Every
//!   hook event bumps the heartbeat, so its age is time since the agent last
//!   did anything. Headless `-p` runs never record a tmux session, and a run
//!   without a session id could never be resumed, so neither qualifies.
//! - A recorded pid that is alive, and ADR-0005's
//!   ([`crate::work::kill_safety`]) notion of a live run — hibernation stops
//!   exactly the processes the kill-safety picker calls `live-run`, never a
//!   second definition of its own. A run hosted in a `root-session` (a
//!   project hub) is never touched.
//! - A [`LaunchRecord`] written at launch by [`record_launch`]. Waking
//!   needs the launch's flags (hooks settings, model) and its window name;
//!   a run launched before this module existed, or adopted by a session tm
//!   did not launch, has none, so it is left alone.
//!
//! # No false finishes
//!
//! The run is marked [`RunStatus::Hibernated`] *before* its agent is
//! stopped, and the agent is stopped with `SIGKILL`: a killed process runs
//! no `SessionEnd` hook or exit handler, so nothing calls `tm runs finish`
//! on it. `tm runs finish` additionally refuses a hibernated row unless
//! forced, for an agent that somehow exits gracefully anyway. The reaper
//! only sweeps `running` rows, so a hibernated one is never reaped either.
//!
//! # Waking
//!
//! [`wake_session`] reopens each hibernated run hosted in a session in a
//! fresh window running [`crate::agent::ResumeSpec::command_line`], records
//! the new pane's pid, and rewrites the session marker the hooks resolve
//! the run id through (the agent's own `SessionEnd` cleanup may have removed
//! it). The row is claimed (`hibernated` → `running`) *before* the window
//! is created, so two concurrent attaches cannot both resume it.

use std::path::Path;

use thiserror::Error;

use crate::agent::ResumeSpec;
use crate::runs::{RunStatus, RunStore, RunStoreError};
use crate::work::audit::SESSION_RUN_ID_ENV;
use crate::work::tmux::{TmuxError, TmuxOps, session_window_names, unique_window_name};

/// `run_events.kind` of the event [`record_launch`] writes.
pub const LAUNCH_EVENT_KIND: &str = "launch";

/// What an interactive launch records so the run can be woken later: the
/// window its agent was started in, and how to resume it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LaunchRecord {
    /// The tmux window the agent was launched in (after
    /// [`unique_window_name`] deduplication, e.g. `work` or `fix-2`).
    pub window: String,
    /// The runner-produced resume recipe; see
    /// [`crate::agent::AgentRunner::resume_spec`].
    pub resume: ResumeSpec,
}

/// A run [`sweep_idle`] hibernated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HibernatedRun {
    /// Row id.
    pub id: i64,
    /// Ticket key.
    pub ticket: String,
}

/// A run [`wake_session`] resumed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WokenRun {
    /// Row id.
    pub id: i64,
    /// The window it was resumed in.
    pub window: String,
}

/// Errors from [`wake_session`].
#[derive(Debug, Error)]
pub enum WakeError {
    /// A [`RunStore`] operation failed.
    #[error(transparent)]
    Store(#[from] RunStoreError),

    /// Shelling out to `tmux` failed.
    #[error(transparent)]
    Tmux(#[from] TmuxError),

    /// The session marker could not be written.
    #[error("failed to write session marker {path}: {source}")]
    Marker {
        /// The marker path.
        path: std::path::PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
}

/// Records `run_id`'s [`LaunchRecord`] as a [`LAUNCH_EVENT_KIND`] event.
/// Called by every interactive launch right after its window is created.
pub fn record_launch(
    store: &RunStore,
    run_id: i64,
    record: &LaunchRecord,
) -> Result<(), RunStoreError> {
    let detail = serde_json::to_string(record).expect("a LaunchRecord always serializes");
    store.add_event(run_id, LAUNCH_EVENT_KIND, Some(&detail))?;
    Ok(())
}

/// The most recent [`LaunchRecord`] recorded for `run_id`, or `None` when
/// it has none (or its detail does not parse).
pub fn launch_record(store: &RunStore, run_id: i64) -> Result<Option<LaunchRecord>, RunStoreError> {
    Ok(store
        .events_for_run(run_id)?
        .iter()
        .rev()
        .filter(|event| event.kind == LAUNCH_EVENT_KIND)
        .find_map(|event| serde_json::from_str(event.detail.as_deref()?).ok()))
}

/// Hibernate every idle interactive run (see the module docs for the exact
/// rule). A no-op when `idle_mins` is `0`, the opt-out.
///
/// For each run: mark it [`RunStatus::Hibernated`], then `kill_pid` its
/// recorded pid (production passes a `SIGKILL` sender — see the module
/// docs' "No false finishes"), then kill its launch window, best-effort
/// (with `remain-on-exit` unset the window has already closed with its
/// pane). tmux errors never fail the sweep: a failed root-session lookup
/// only means no run is excluded on that ground, and the store is the
/// authority on what was hibernated.
pub fn sweep_idle(
    store: &RunStore,
    tmux: &dyn TmuxOps,
    idle_mins: u64,
    pid_alive: &dyn Fn(u32) -> bool,
    kill_pid: &dyn Fn(u32),
) -> Result<Vec<HibernatedRun>, RunStoreError> {
    if idle_mins == 0 {
        return Ok(Vec::new());
    }
    let candidates = store.idle_interactive_runs(idle_mins)?;
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let root_sessions = tmux.root_session_targets().unwrap_or_default();

    let mut hibernated = Vec::new();
    for run in candidates {
        let (Some(pid), Some(session)) = (run.pid, run.tmux_session.as_deref()) else {
            continue;
        };
        if !pid_alive(pid) || !crate::work::kill_safety::run_is_live(&run, pid_alive) {
            continue;
        }
        if root_sessions.iter().any(|root| root == session) {
            continue;
        }
        let Some(record) = launch_record(store, run.id)? else {
            continue;
        };
        if !store.hibernate_run(run.id)? {
            continue;
        }
        kill_pid(pid);
        let _ = tmux.kill_window(session, &record.window);
        hibernated.push(HibernatedRun {
            id: run.id,
            ticket: run.ticket,
        });
    }
    Ok(hibernated)
}

/// Resume every hibernated run hosted in tmux session `session_name`; see
/// the module docs' "Waking" section. Returns the runs woken, oldest first;
/// empty (touching no tmux at all) when there were none.
///
/// Each woken run gets a window named after its launch window, deduplicated
/// against the session's current windows, running its
/// [`ResumeSpec::command_line`] with [`SESSION_RUN_ID_ENV`] set like the
/// original launch. The session is recreated when it no longer exists. The
/// first woken window is selected so an attach that follows lands on it.
///
/// A run whose window cannot be created is put back to hibernated before
/// the error is returned, so a later attach can try again.
pub fn wake_session(
    store: &RunStore,
    tmux: &dyn TmuxOps,
    session_name: &str,
    sessions_dir: &Path,
) -> Result<Vec<WokenRun>, WakeError> {
    let mut hibernated: Vec<_> = store
        .all_runs()?
        .into_iter()
        .filter(|run| {
            run.status == RunStatus::Hibernated && run.tmux_session.as_deref() == Some(session_name)
        })
        .collect();
    if hibernated.is_empty() {
        return Ok(Vec::new());
    }
    // `all_runs` is newest-first; resume in launch order.
    hibernated.reverse();

    let windows = tmux.list_windows()?;
    let mut existing = session_window_names(&windows, session_name);

    let mut woken = Vec::new();
    for run in hibernated {
        let Some(session_id) = run.session_id.as_deref() else {
            continue;
        };
        let Some(record) = launch_record(store, run.id)? else {
            continue;
        };
        if !store.wake_run(run.id, None)? {
            continue;
        }

        let window = unique_window_name(&record.window, &existing);
        let command = record.resume.command_line(session_id);
        let env = [(SESSION_RUN_ID_ENV.to_string(), run.id.to_string())];
        let created = if existing.is_empty() {
            tmux.new_session_with_command(session_name, &run.worktree, &window, &env, &command)
        } else {
            tmux.new_window_with_command(session_name, &window, &run.worktree, &env, &command)
        };
        if let Err(err) = created {
            let _ = store.hibernate_run(run.id);
            return Err(err.into());
        }
        existing.push(window.clone());

        if let Ok(Some(pid)) = tmux.pane_pid(session_name, &window) {
            store.update_pid(run.id, pid)?;
        }
        write_marker(sessions_dir, session_id, run.id)?;

        woken.push(WokenRun { id: run.id, window });
    }

    if let Some(first) = woken.first() {
        tmux.select_window(session_name, &first.window)?;
    }
    Ok(woken)
}

/// Points the hooks' session marker for `session_id` back at `run_id` (see
/// [`crate::runs::session`]); the resumed agent's events would otherwise
/// find no run to report to, its heartbeat would never move, and the next
/// sweep would hibernate it mid-conversation.
fn write_marker(sessions_dir: &Path, session_id: &str, run_id: i64) -> Result<(), WakeError> {
    let path = sessions_dir.join(session_id);
    std::fs::create_dir_all(sessions_dir)
        .and_then(|()| std::fs::write(&path, run_id.to_string()))
        .map_err(|source| WakeError::Marker { path, source })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::runs::StartRun;
    use crate::work::tmux::{FakeTmuxOps, TmuxCall, TmuxWindow};
    use tempfile::tempdir;

    const SESSION: &str = "tm-proj-proj-1";

    fn spec() -> ResumeSpec {
        ResumeSpec {
            program: "agent".to_string(),
            resume_flag: "--resume".to_string(),
            args: vec!["--settings".to_string(), "/hooks.json".to_string()],
            env_remove: vec!["SECRET".to_string()],
        }
    }

    fn record() -> LaunchRecord {
        LaunchRecord {
            window: "work".to_string(),
            resume: spec(),
        }
    }

    fn open_store(dir: &Path) -> RunStore {
        RunStore::open(&dir.join("runs.db")).unwrap()
    }

    /// A tmux-hosted, registered, launch-recorded run whose heartbeat is
    /// `idle_mins` old.
    fn interactive_run(store: &RunStore, idle_mins: i64, with_record: bool) -> i64 {
        let id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                scope: String::new(),
                lane: "backend".to_string(),
                worktree: "/wt/proj-1".to_string(),
                branch: None,
                pid: Some(4242),
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();
        if with_record {
            record_launch(store, id, &record()).unwrap();
        }
        store.update_session_id(id, "sess-1").unwrap();
        store.update_tmux_session(id, SESSION).unwrap();
        backdate(store, id, idle_mins);
        id
    }

    fn backdate(store: &RunStore, id: i64, mins: i64) {
        store.backdate_heartbeat_for_tests(id, mins);
    }

    #[test]
    fn launch_record_round_trips_through_the_event_log() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = interactive_run(&store, 0, true);

        assert_eq!(launch_record(&store, id).unwrap(), Some(record()));
    }

    #[test]
    fn launch_record_is_none_for_a_run_without_one() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = interactive_run(&store, 0, false);

        assert_eq!(launch_record(&store, id).unwrap(), None);
    }

    #[test]
    fn sweep_hibernates_an_idle_live_run_then_kills_its_agent_and_window() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = interactive_run(&store, 90, true);
        let tmux = FakeTmuxOps::new();
        let killed = RefCell::new(Vec::new());

        let hibernated = sweep_idle(&store, &tmux, 60, &|_| true, &|pid| {
            // The row must already read hibernated when the agent dies, or
            // its exit could race a finish.
            assert_eq!(
                store.run_by_id(id).unwrap().unwrap().status,
                RunStatus::Hibernated
            );
            killed.borrow_mut().push(pid);
        })
        .unwrap();

        assert_eq!(
            hibernated,
            vec![HibernatedRun {
                id,
                ticket: "PROJ-1".to_string()
            }]
        );
        assert_eq!(*killed.borrow(), vec![4242]);
        assert!(tmux.calls().contains(&TmuxCall::KillWindow {
            name: SESSION.to_string(),
            window: "work".to_string()
        }));
    }

    #[test]
    fn sweep_is_a_no_op_when_the_threshold_is_zero() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = interactive_run(&store, 90, true);

        let hibernated =
            sweep_idle(&store, &FakeTmuxOps::new(), 0, &|_| true, &|_| panic!()).unwrap();

        assert!(hibernated.is_empty());
        assert_eq!(
            store.run_by_id(id).unwrap().unwrap().status,
            RunStatus::Running
        );
    }

    #[test]
    fn sweep_leaves_fresh_dead_and_unrecorded_runs_alone() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let fresh = interactive_run(&store, 5, true);
        let unrecorded = interactive_run(&store, 90, false);

        let hibernated =
            sweep_idle(&store, &FakeTmuxOps::new(), 60, &|_| true, &|_| panic!()).unwrap();
        assert!(hibernated.is_empty());

        let dead = sweep_idle(&store, &FakeTmuxOps::new(), 60, &|_| false, &|_| panic!()).unwrap();
        assert!(dead.is_empty(), "a dead pid is the reaper's business");

        for id in [fresh, unrecorded] {
            assert_eq!(
                store.run_by_id(id).unwrap().unwrap().status,
                RunStatus::Running
            );
        }
    }

    #[test]
    fn sweep_never_touches_a_run_hosted_in_a_root_session() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        interactive_run(&store, 90, true);
        let tmux = FakeTmuxOps::new().with_root_session_targets(Ok(vec![SESSION.to_string()]));

        let hibernated = sweep_idle(&store, &tmux, 60, &|_| true, &|_| panic!()).unwrap();

        assert!(hibernated.is_empty());
    }

    fn hibernated_run(store: &RunStore) -> i64 {
        let id = interactive_run(store, 90, true);
        store.hibernate_run(id).unwrap();
        id
    }

    #[test]
    fn wake_reopens_the_window_with_the_resume_command_and_marks_the_run_running() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = hibernated_run(&store);
        let tmux = FakeTmuxOps::new()
            .with_list_windows(Ok(vec![TmuxWindow {
                session: SESSION.to_string(),
                name: "shell".to_string(),
                dead: false,
            }]))
            .with_pane_pid(Ok(Some(9191)));
        let sessions = dir.path().join("sessions");

        let woken = wake_session(&store, &tmux, SESSION, &sessions).unwrap();

        assert_eq!(
            woken,
            vec![WokenRun {
                id,
                window: "work".to_string()
            }]
        );
        assert!(tmux.calls().contains(&TmuxCall::NewWindowWithCommand {
            name: SESSION.to_string(),
            window_name: "work".to_string(),
            dir: "/wt/proj-1".to_string(),
            env: vec![(SESSION_RUN_ID_ENV.to_string(), id.to_string())],
            command: spec().command_line("sess-1"),
        }));
        let run = store.run_by_id(id).unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Running);
        assert_eq!(run.pid, Some(9191));
        assert_eq!(
            std::fs::read_to_string(sessions.join("sess-1")).unwrap(),
            id.to_string()
        );
        assert_eq!(
            tmux.calls().last(),
            Some(&TmuxCall::SelectWindow {
                name: SESSION.to_string(),
                window: "work".to_string()
            })
        );
    }

    #[test]
    fn wake_recreates_the_session_when_it_is_gone() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = hibernated_run(&store);
        let tmux = FakeTmuxOps::new();

        wake_session(&store, &tmux, SESSION, &dir.path().join("sessions")).unwrap();

        assert!(tmux.calls().contains(&TmuxCall::NewSessionWithCommand {
            name: SESSION.to_string(),
            dir: "/wt/proj-1".to_string(),
            window_name: "work".to_string(),
            env: vec![(SESSION_RUN_ID_ENV.to_string(), id.to_string())],
            command: spec().command_line("sess-1"),
        }));
    }

    #[test]
    fn wake_ignores_running_runs_and_other_sessions() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        interactive_run(&store, 90, true);
        let elsewhere = hibernated_run(&store);
        store
            .update_tmux_session(elsewhere, "tm-proj-proj-2")
            .unwrap();
        let tmux = FakeTmuxOps::new();

        let woken = wake_session(&store, &tmux, SESSION, &dir.path().join("sessions")).unwrap();

        assert!(woken.is_empty());
        assert!(tmux.calls().is_empty(), "nothing to wake touches no tmux");
    }
}
