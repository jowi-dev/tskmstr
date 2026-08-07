//! Session-run registration: the marker-file protocol behind
//! `docs/plans/session-usage.md`'s "Session registration" design.
//!
//! Interactive Claude Code sessions (`tm ticket audit`, `tm ticket create`)
//! have no wrapper process to start/finish a [`crate::runs::RunStore`] run
//! the way `tm work run` does — there is only a sequence of `tm` invocations
//! sharing one Claude Code session. This module gives each such session a
//! run of its own, keyed by the session's identity rather than by an
//! explicit `--run-id` flag:
//!
//! - [`register_session`] starts (or reuses) a run and writes a **marker
//!   file** at `<sessions dir>/<session_id>` containing the bare run id
//!   (`tm`'s usual bare-output style — trivially `cat`-able from shell
//!   hooks). [`sessions_dir`] resolves the marker directory the same way
//!   [`crate::runs::default_db_path`] resolves the database path, honoring
//!   `XDG_DATA_HOME`.
//! - The telemetry hooks (`hooks/tm-*.sh`) read this marker by session id to
//!   find the run id when `TSKMSTR_RUN_ID` is unset — see the plan's "Hook
//!   gating: marker fallback" section. This module does not touch the
//!   hooks; it only produces the files they read.
//! - [`finish_session`] finishes the marker's run and unlinks the marker.
//!
//! **Adoption of board-launched sessions.** `docs/plans/board-audits.md`'s
//! "Adoption" design adds a second way a run can get a marker: a session
//! [`crate::work::audit::launch_audit`] starts already has a run row
//! (`pid = None`, created before `claude` even booted) and passes its id via
//! `TSKMSTR_SESSION_RUN_ID` (see [`SessionEnv::session_run_id`]). Before the
//! marker/new-run logic above runs at all, [`register_session`] checks
//! whether that env var points at a still-[`RunStatus::Running`] run whose
//! `kind`/`ticket` match the call — and if so, *adopts* it: writes the
//! marker pointing at that run id, stamps `session_id`, and stamps `pid`
//! when known, without ever calling [`RunStore::start_run`]. Any mismatch
//! (missing run, wrong status, wrong kind/ticket) falls through to the
//! existing marker-reuse/new-run behavior unchanged — the env var is
//! advisory, the marker stays the source of truth.
//!
//! `CLAUDE_CODE_SESSION_ID` and `CLAUDE_PID` (read by [`SessionEnv`]) are
//! **observed but undocumented** environment variables Claude Code sets for
//! Bash-tool subprocesses; anything reading them must degrade to a silent
//! no-op when they're absent, since their availability is not a documented
//! guarantee. [`SessionEnv::from_process_env`] does exactly that: absence or
//! garbage in either becomes `None`, never an error.
//!
//! `pid = CLAUDE_PID` matters beyond identification: [`crate::runs::RunStore::reap`]
//! reaps a stale-heartbeat running run on staleness alone when its `pid` is
//! `NULL`, but skips it while the pid is alive. An interactive session idles
//! between tool calls for long stretches, so a session run registered
//! without a pid would be reaped `failed` after one quiet spell. Recording
//! `CLAUDE_PID` at start keeps an idle-but-alive session's run alive too.
//!
//! **Error-handling contract**: every fallible operation here returns a real
//! [`SessionError`] rather than swallowing failures — this module does not
//! decide that telemetry is optional. The plan requires callers (the ticket
//! commands) to degrade silently on failure so a broken runs DB or an
//! unwritable marker directory never blocks a ticket command's own output or
//! exit code; that swallowing is the *caller's* job, done at the call site.

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::runs::{FinishRun, RunStatus, RunStore, RunStoreError, StartRun};

/// Errors returned by this module's functions.
#[derive(Debug, Error)]
pub enum SessionError {
    /// A [`RunStore`] operation failed.
    #[error(transparent)]
    Store(#[from] RunStoreError),

    /// The sessions directory could not be created.
    #[error("failed to create sessions directory {path}: {source}")]
    CreateDir {
        /// Directory that could not be created.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// A marker file could not be written.
    #[error("failed to write session marker {path}: {source}")]
    WriteMarker {
        /// Marker path that could not be written.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// A marker file could not be read.
    #[error("failed to read session markers directory {path}: {source}")]
    ReadMarker {
        /// Directory that could not be read.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// A marker file could not be removed.
    #[error("failed to remove session marker {path}: {source}")]
    RemoveMarker {
        /// Marker path that could not be removed.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

/// The subset of the process environment [`register_session`]/
/// [`finish_session`] act on, gathered into one struct so tests can
/// construct it directly rather than mocking environment variables.
#[derive(Debug, Clone)]
pub struct SessionEnv {
    /// `CLAUDE_CODE_SESSION_ID`: the Claude Code session UUID, matching the
    /// `session_id` hooks receive on stdin. Observed but undocumented (see
    /// module docs); `None` when unset or empty.
    pub session_id: Option<String>,
    /// `CLAUDE_PID`: the Claude process pid. Observed but undocumented;
    /// `None` when unset, empty, or unparseable as a `u32`.
    pub claude_pid: Option<u32>,
    /// `TSKMSTR_RUN_ID`: set by the `tm work run` lane wrapper. When
    /// present, a lane run already owns telemetry for this process tree, so
    /// session registration is a no-op (see [`register_session`]).
    pub lane_run_id: Option<String>,
    /// `TSKMSTR_SESSION_RUN_ID`: set by
    /// [`crate::work::audit::launch_audit`] on a board-launched session,
    /// naming the pre-registered run id for [`register_session`] to adopt
    /// (see the module docs' "Adoption of board-launched sessions"
    /// section). `None` when unset, empty, or unparseable as an `i64` —
    /// this is advisory input, never load-bearing on its own.
    pub session_run_id: Option<i64>,
    /// The current working directory, recorded as a session run's
    /// `worktree`.
    pub cwd: PathBuf,
}

/// Reads an environment variable, treating an empty string the same as
/// unset — both count as absent.
fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

impl SessionEnv {
    /// Reads [`SessionEnv`] from the current process's environment and
    /// working directory.
    ///
    /// `CLAUDE_CODE_SESSION_ID` and `TSKMSTR_RUN_ID` are absent when unset or
    /// empty; `CLAUDE_PID` and `TSKMSTR_SESSION_RUN_ID` are additionally
    /// absent when their values fail to parse (as a `u32`/`i64`
    /// respectively). `cwd` falls back to `PathBuf::new()` (an empty path)
    /// if [`std::env::current_dir`] fails, rather than erroring — this is
    /// telemetry, not a load-bearing path.
    pub fn from_process_env() -> Self {
        SessionEnv {
            session_id: non_empty_env("CLAUDE_CODE_SESSION_ID"),
            claude_pid: non_empty_env("CLAUDE_PID").and_then(|v| v.parse().ok()),
            lane_run_id: non_empty_env("TSKMSTR_RUN_ID"),
            session_run_id: non_empty_env("TSKMSTR_SESSION_RUN_ID").and_then(|v| v.parse().ok()),
            cwd: std::env::current_dir().unwrap_or_default(),
        }
    }
}

/// Returns the directory session marker files live in:
/// `$XDG_DATA_HOME/tskmstr/sessions` when `xdg_data_home` is set, otherwise
/// `home/.local/share/tskmstr/sessions`. Mirrors
/// [`crate::runs::default_db_path`]'s resolution (same base directory,
/// `sessions` instead of `runs.db`).
pub fn sessions_dir(home: &Path, xdg_data_home: Option<&Path>) -> PathBuf {
    match xdg_data_home {
        Some(xdg) => xdg.join("tskmstr").join("sessions"),
        None => home
            .join(".local")
            .join("share")
            .join("tskmstr")
            .join("sessions"),
    }
}

/// Convenience wrapper around [`sessions_dir`] that reads `HOME` and
/// `XDG_DATA_HOME` from the current process's environment.
///
/// # Panics
///
/// Panics if `HOME` is unset — same expectation
/// `crate::runs::default_db_path` callers already carry, since a process
/// with no `HOME` has no sensible default data directory.
pub fn sessions_dir_from_process_env() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME must be set");
    let xdg = non_empty_env("XDG_DATA_HOME");
    sessions_dir(Path::new(&home), xdg.as_deref().map(Path::new))
}

/// Path of the marker file for `session_id` inside `dir`.
fn marker_path(dir: &Path, session_id: &str) -> PathBuf {
    dir.join(session_id)
}

/// Reads a marker file's contents and parses it as a run id. Returns `None`
/// (rather than an error) when the file is missing, unreadable, or its
/// contents don't parse as an `i64` — every one of those cases means "no
/// usable marker", which callers treat identically.
fn read_marker(path: &Path) -> Option<i64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Adopts a pre-registered run: creates `dir` if needed, writes the marker
/// for `session_id` pointing at `run_id`, stamps `run_id`'s `session_id`,
/// and stamps its `pid` when `claude_pid` is known. Shared by
/// [`register_session`]'s adoption path; see the module docs' "Adoption of
/// board-launched sessions" section.
fn adopt_run(
    store: &RunStore,
    dir: &Path,
    session_id: &str,
    run_id: i64,
    claude_pid: Option<u32>,
) -> Result<i64, SessionError> {
    std::fs::create_dir_all(dir).map_err(|source| SessionError::CreateDir {
        path: dir.to_path_buf(),
        source,
    })?;

    let path = marker_path(dir, session_id);
    std::fs::write(&path, run_id.to_string()).map_err(|source| SessionError::WriteMarker {
        path: path.clone(),
        source,
    })?;

    store.update_session_id(run_id, session_id)?;
    if let Some(pid) = claude_pid {
        store.update_pid(run_id, pid)?;
    }

    Ok(run_id)
}

/// Registers (or reuses, or adopts) a session run for `kind`/`ticket`, per
/// `docs/plans/session-usage.md`'s "Session registration" design and
/// `docs/plans/board-audits.md`'s "Adoption" design.
///
/// Returns:
/// - `Ok(None)` — no-op: `env.session_id` is absent, or `env.lane_run_id` is
///   present (a lane run already owns telemetry).
/// - `Ok(Some(id))` — adopted `env.session_run_id`'s pre-registered run: it
///   exists, is [`RunStatus::Running`], and matches `kind`/`ticket`. The
///   marker is written pointing at it, and its `session_id`/`pid` are
///   stamped (see the module docs' "Adoption of board-launched sessions").
/// - `Ok(Some(id))` — reused the existing run at the session's marker,
///   because it is [`RunStatus::Running`] with the same `kind` and `ticket`.
/// - `Ok(Some(id))` — started a fresh run and wrote the marker, either
///   because there was no marker, the marker's run wasn't running, or it was
///   running a *different* `kind`/`ticket` (in which case that old run is
///   first finished [`RunStatus::Done`]).
///
/// After a fresh start, opportunistically sweeps sibling markers in `dir`
/// whose run is no longer running (or fails to resolve at all), to keep the
/// directory from accumulating one file per session forever. The marker
/// just written is never swept. Adoption does not trigger a sweep — it
/// writes exactly one marker, same as any other registration, and the next
/// fresh start will sweep it in its turn once it finishes.
pub fn register_session(
    store: &RunStore,
    dir: &Path,
    env: &SessionEnv,
    kind: &str,
    ticket: &str,
) -> Result<Option<i64>, SessionError> {
    let Some(session_id) = env.session_id.as_deref() else {
        return Ok(None);
    };
    if env.lane_run_id.is_some() {
        return Ok(None);
    }

    if let Some(candidate_id) = env.session_run_id
        && let Some(run) = store.run_by_id(candidate_id)?
        && run.status == RunStatus::Running
        && run.kind == kind
        && run.ticket == ticket
    {
        return adopt_run(store, dir, session_id, candidate_id, env.claude_pid).map(Some);
    }

    let path = marker_path(dir, session_id);

    if let Some(existing_id) = read_marker(&path)
        && let Some(run) = store.run_by_id(existing_id)?
        && run.status == RunStatus::Running
    {
        if run.kind == kind && run.ticket == ticket {
            return Ok(Some(existing_id));
        }
        store.finish_run(
            existing_id,
            &FinishRun {
                status: RunStatus::Done,
                ..FinishRun::default()
            },
        )?;
    }

    std::fs::create_dir_all(dir).map_err(|source| SessionError::CreateDir {
        path: dir.to_path_buf(),
        source,
    })?;

    let new_id = store.start_run(&StartRun {
        ticket: ticket.to_string(),
        lane: kind.to_string(),
        worktree: env.cwd.display().to_string(),
        branch: None,
        pid: env.claude_pid,
        kind: kind.to_string(),
    })?;
    store.update_session_id(new_id, session_id)?;

    std::fs::write(&path, new_id.to_string()).map_err(|source| SessionError::WriteMarker {
        path: path.clone(),
        source,
    })?;

    sweep(store, dir, &path)?;

    Ok(Some(new_id))
}

/// Removes every marker in `dir` other than `just_written` whose contents
/// don't resolve to a currently [`RunStatus::Running`] run — either because
/// the contents don't parse, the run doesn't exist, or the run has already
/// finished. Best-effort: a directory read error is surfaced, but an
/// individual marker that fails to `remove_file` is skipped rather than
/// aborting the sweep (another process may have already cleaned it up).
fn sweep(store: &RunStore, dir: &Path, just_written: &Path) -> Result<(), SessionError> {
    let entries = std::fs::read_dir(dir).map_err(|source| SessionError::ReadMarker {
        path: dir.to_path_buf(),
        source,
    })?;

    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path == just_written {
            continue;
        }

        let still_running = match read_marker(&path) {
            Some(id) => matches!(
                store.run_by_id(id)?,
                Some(run) if run.status == RunStatus::Running
            ),
            None => false,
        };

        if !still_running {
            let _ = std::fs::remove_file(&path);
        }
    }

    Ok(())
}

/// Finishes the session's marker run and unlinks the marker, per
/// `docs/plans/session-usage.md`'s `finish_session`.
///
/// Returns `Ok(None)` (a no-op) when `env.session_id` is absent,
/// `env.lane_run_id` is present, there is no marker (or the marker's
/// contents don't parse) for the session, or the marker's run is not for
/// this `kind` and `ticket` — every case where there is no run this call is
/// responsible for finishing. The kind/ticket match matters because the
/// marker tracks the session's *latest* registration: a session that audits
/// PROJ-1, then reads PROJ-2 for context, has its marker pointing at
/// PROJ-2's run, and recording PROJ-1's verdict must leave that run (and
/// the marker) alone. Otherwise finishes the marker's run with `status`
/// (all other [`FinishRun`] fields left `None`, so `finish_run`'s
/// `COALESCE` semantics preserve whatever was already recorded), unlinks
/// the marker, and returns `Ok(Some(id))`.
pub fn finish_session(
    store: &RunStore,
    dir: &Path,
    env: &SessionEnv,
    kind: &str,
    ticket: &str,
    status: RunStatus,
) -> Result<Option<i64>, SessionError> {
    let Some(session_id) = env.session_id.as_deref() else {
        return Ok(None);
    };
    if env.lane_run_id.is_some() {
        return Ok(None);
    }

    let path = marker_path(dir, session_id);
    let Some(run_id) = read_marker(&path) else {
        return Ok(None);
    };

    match store.run_by_id(run_id)? {
        Some(run) if run.kind == kind && run.ticket == ticket => {}
        _ => return Ok(None),
    }

    store.finish_run(
        run_id,
        &FinishRun {
            status,
            ..FinishRun::default()
        },
    )?;

    std::fs::remove_file(&path).map_err(|source| SessionError::RemoveMarker {
        path: path.clone(),
        source,
    })?;

    Ok(Some(run_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn open_store(dir: &Path) -> RunStore {
        RunStore::open(&dir.join("runs.db")).expect("open should succeed")
    }

    fn env_with_session(session_id: &str) -> SessionEnv {
        SessionEnv {
            session_id: Some(session_id.to_string()),
            claude_pid: Some(4242),
            lane_run_id: None,
            session_run_id: None,
            cwd: PathBuf::from("/tmp/wt"),
        }
    }

    #[test]
    fn register_session_is_a_noop_without_a_session_id() {
        let db_dir = tempdir().unwrap();
        let markers_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());
        let env = SessionEnv {
            session_id: None,
            claude_pid: None,
            lane_run_id: None,
            session_run_id: None,
            cwd: PathBuf::from("/tmp/wt"),
        };

        let result = register_session(&store, markers_dir.path(), &env, "audit", "PROJ-1")
            .expect("should not error");

        assert_eq!(result, None);
        assert!(store.list_runs().unwrap().is_empty());
    }

    #[test]
    fn register_session_is_a_noop_when_a_lane_run_owns_telemetry() {
        let db_dir = tempdir().unwrap();
        let markers_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());
        let mut env = env_with_session("sess-1");
        env.lane_run_id = Some("77".to_string());

        let result = register_session(&store, markers_dir.path(), &env, "audit", "PROJ-1")
            .expect("should not error");

        assert_eq!(result, None);
        assert!(store.list_runs().unwrap().is_empty());
    }

    #[test]
    fn register_session_starts_a_fresh_run_and_writes_the_marker() {
        let db_dir = tempdir().unwrap();
        let markers_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());
        let env = env_with_session("sess-1");

        let id = register_session(&store, markers_dir.path(), &env, "audit", "PROJ-1")
            .expect("should not error")
            .expect("expected a new run id");

        let run = store.run_by_id(id).unwrap().expect("expected a run row");
        assert_eq!(run.ticket, "PROJ-1");
        assert_eq!(run.kind, "audit");
        assert_eq!(run.lane, "audit");
        assert_eq!(run.status, RunStatus::Running);
        assert_eq!(run.pid, Some(4242));
        assert_eq!(run.session_id, Some("sess-1".to_string()));
        assert_eq!(run.worktree, "/tmp/wt");

        let marker = markers_dir.path().join("sess-1");
        assert!(marker.exists());
        let contents = std::fs::read_to_string(&marker).unwrap();
        assert_eq!(contents, id.to_string());
    }

    // --- adoption of a pre-registered (board-launched) run ---

    #[test]
    fn register_session_adopts_a_matching_preregistered_run() {
        let db_dir = tempdir().unwrap();
        let markers_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());

        let pre_id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "audit".to_string(),
                worktree: "/repo/axiom".to_string(),
                branch: None,
                pid: None,
                kind: "audit".to_string(),
            })
            .unwrap();

        let mut env = env_with_session("sess-1");
        env.session_run_id = Some(pre_id);

        let id = register_session(&store, markers_dir.path(), &env, "audit", "PROJ-1")
            .expect("should not error")
            .expect("expected the adopted run id");

        assert_eq!(id, pre_id);
        assert_eq!(store.list_runs().unwrap().len(), 1, "no new run created");

        let run = store.run_by_id(pre_id).unwrap().expect("run row exists");
        assert_eq!(run.session_id, Some("sess-1".to_string()));
        assert_eq!(run.pid, Some(4242));
        assert_eq!(run.status, RunStatus::Running);

        let marker = markers_dir.path().join("sess-1");
        let contents = std::fs::read_to_string(&marker).unwrap();
        assert_eq!(contents, pre_id.to_string());
    }

    #[test]
    fn register_session_adoption_falls_through_on_kind_mismatch() {
        let db_dir = tempdir().unwrap();
        let markers_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());

        let pre_id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "create".to_string(),
                worktree: "/repo/axiom".to_string(),
                branch: None,
                pid: None,
                kind: "create".to_string(),
            })
            .unwrap();

        let mut env = env_with_session("sess-1");
        env.session_run_id = Some(pre_id);

        let id = register_session(&store, markers_dir.path(), &env, "audit", "PROJ-1")
            .unwrap()
            .expect("should fall through to a fresh run");

        assert_ne!(id, pre_id, "must not adopt a run of the wrong kind");
        assert_eq!(
            store.run_by_id(pre_id).unwrap().unwrap().status,
            RunStatus::Running,
            "the mismatched run is left untouched, not finished"
        );
        assert_eq!(store.run_by_id(id).unwrap().unwrap().kind, "audit");
    }

    #[test]
    fn register_session_adoption_falls_through_on_ticket_mismatch() {
        let db_dir = tempdir().unwrap();
        let markers_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());

        let pre_id = store
            .start_run(&StartRun {
                ticket: "PROJ-2".to_string(),
                lane: "audit".to_string(),
                worktree: "/repo/axiom".to_string(),
                branch: None,
                pid: None,
                kind: "audit".to_string(),
            })
            .unwrap();

        let mut env = env_with_session("sess-1");
        env.session_run_id = Some(pre_id);

        let id = register_session(&store, markers_dir.path(), &env, "audit", "PROJ-1")
            .unwrap()
            .expect("should fall through to a fresh run");

        assert_ne!(id, pre_id, "must not adopt a run for a different ticket");
        assert_eq!(store.run_by_id(id).unwrap().unwrap().ticket, "PROJ-1");
    }

    #[test]
    fn register_session_adoption_falls_through_on_missing_run() {
        let db_dir = tempdir().unwrap();
        let markers_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());

        let mut env = env_with_session("sess-1");
        env.session_run_id = Some(999_999);

        let id = register_session(&store, markers_dir.path(), &env, "audit", "PROJ-1")
            .unwrap()
            .expect("should fall through to a fresh run");

        let run = store.run_by_id(id).unwrap().unwrap();
        assert_eq!(run.ticket, "PROJ-1");
        assert_eq!(run.kind, "audit");
    }

    #[test]
    fn register_session_adoption_falls_through_on_finished_run() {
        let db_dir = tempdir().unwrap();
        let markers_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());

        let pre_id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "audit".to_string(),
                worktree: "/repo/axiom".to_string(),
                branch: None,
                pid: None,
                kind: "audit".to_string(),
            })
            .unwrap();
        store.finish_run(pre_id, &FinishRun::default()).unwrap();

        let mut env = env_with_session("sess-1");
        env.session_run_id = Some(pre_id);

        let id = register_session(&store, markers_dir.path(), &env, "audit", "PROJ-1")
            .unwrap()
            .expect("should fall through to a fresh run");

        assert_ne!(id, pre_id, "must not adopt a run that already finished");
        assert_eq!(
            store.run_by_id(id).unwrap().unwrap().status,
            RunStatus::Running
        );
    }

    #[test]
    fn register_session_reuses_the_running_run_for_the_same_kind_and_ticket() {
        let db_dir = tempdir().unwrap();
        let markers_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());
        let env = env_with_session("sess-1");

        let first = register_session(&store, markers_dir.path(), &env, "audit", "PROJ-1")
            .unwrap()
            .unwrap();
        let second = register_session(&store, markers_dir.path(), &env, "audit", "PROJ-1")
            .unwrap()
            .unwrap();

        assert_eq!(first, second);
        assert_eq!(store.list_runs().unwrap().len(), 1);
    }

    #[test]
    fn register_session_finishes_the_old_run_when_the_ticket_changes() {
        let db_dir = tempdir().unwrap();
        let markers_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());
        let env = env_with_session("sess-1");

        let first = register_session(&store, markers_dir.path(), &env, "audit", "PROJ-1")
            .unwrap()
            .unwrap();
        let second = register_session(&store, markers_dir.path(), &env, "audit", "PROJ-2")
            .unwrap()
            .unwrap();

        assert_ne!(first, second);

        let old_run = store.run_by_id(first).unwrap().unwrap();
        assert_eq!(old_run.status, RunStatus::Done);

        let new_run = store.run_by_id(second).unwrap().unwrap();
        assert_eq!(new_run.status, RunStatus::Running);
        assert_eq!(new_run.ticket, "PROJ-2");

        let marker = markers_dir.path().join("sess-1");
        let contents = std::fs::read_to_string(&marker).unwrap();
        assert_eq!(contents, second.to_string());
    }

    #[test]
    fn register_session_finishes_the_old_run_when_the_kind_changes() {
        let db_dir = tempdir().unwrap();
        let markers_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());
        let env = env_with_session("sess-1");

        let first = register_session(&store, markers_dir.path(), &env, "audit", "PROJ-1")
            .unwrap()
            .unwrap();
        let second = register_session(&store, markers_dir.path(), &env, "create", "PROJ-1")
            .unwrap()
            .unwrap();

        assert_ne!(first, second);
        assert_eq!(
            store.run_by_id(first).unwrap().unwrap().status,
            RunStatus::Done
        );
        assert_eq!(store.run_by_id(second).unwrap().unwrap().kind, "create");
    }

    #[test]
    fn register_session_starts_fresh_when_the_marker_points_at_a_finished_run() {
        let db_dir = tempdir().unwrap();
        let markers_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());
        let env = env_with_session("sess-1");

        let first = register_session(&store, markers_dir.path(), &env, "audit", "PROJ-1")
            .unwrap()
            .unwrap();
        store.finish_run(first, &FinishRun::default()).unwrap();

        let second = register_session(&store, markers_dir.path(), &env, "audit", "PROJ-1")
            .unwrap()
            .unwrap();

        assert_ne!(first, second);
        assert_eq!(
            store.run_by_id(second).unwrap().unwrap().status,
            RunStatus::Running
        );
    }

    #[test]
    fn register_session_sweeps_markers_of_finished_runs_but_keeps_the_fresh_one() {
        let db_dir = tempdir().unwrap();
        let markers_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());

        // A stale marker pointing at an already-finished run.
        let stale_env = env_with_session("sess-stale");
        let stale_id = register_session(&store, markers_dir.path(), &stale_env, "audit", "OLD-1")
            .unwrap()
            .unwrap();
        store.finish_run(stale_id, &FinishRun::default()).unwrap();

        // A marker with garbage contents that doesn't parse as an id.
        std::fs::write(markers_dir.path().join("sess-garbage"), "not-a-number").unwrap();

        // A fresh registration should sweep both of the above.
        let fresh_env = env_with_session("sess-fresh");
        let fresh_id = register_session(&store, markers_dir.path(), &fresh_env, "audit", "NEW-1")
            .unwrap()
            .unwrap();

        assert!(!markers_dir.path().join("sess-stale").exists());
        assert!(!markers_dir.path().join("sess-garbage").exists());
        let fresh_marker = markers_dir.path().join("sess-fresh");
        assert!(fresh_marker.exists());
        assert_eq!(
            std::fs::read_to_string(&fresh_marker).unwrap(),
            fresh_id.to_string()
        );
    }

    #[test]
    fn register_session_sweep_does_not_touch_markers_of_still_running_runs() {
        let db_dir = tempdir().unwrap();
        let markers_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());

        let other_env = env_with_session("sess-other");
        register_session(&store, markers_dir.path(), &other_env, "audit", "OTHER-1")
            .unwrap()
            .unwrap();

        let fresh_env = env_with_session("sess-fresh");
        register_session(&store, markers_dir.path(), &fresh_env, "audit", "NEW-1")
            .unwrap()
            .unwrap();

        assert!(markers_dir.path().join("sess-other").exists());
    }

    #[test]
    fn finish_session_finishes_the_run_and_unlinks_the_marker() {
        let db_dir = tempdir().unwrap();
        let markers_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());
        let env = env_with_session("sess-1");

        let id = register_session(&store, markers_dir.path(), &env, "audit", "PROJ-1")
            .unwrap()
            .unwrap();

        let finished = finish_session(
            &store,
            markers_dir.path(),
            &env,
            "audit",
            "PROJ-1",
            RunStatus::Done,
        )
        .unwrap();

        assert_eq!(finished, Some(id));
        assert_eq!(
            store.run_by_id(id).unwrap().unwrap().status,
            RunStatus::Done
        );
        assert!(!markers_dir.path().join("sess-1").exists());
    }

    #[test]
    fn finish_session_ignores_a_marker_run_for_a_different_ticket() {
        let db_dir = tempdir().unwrap();
        let markers_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());
        let env = env_with_session("sess-1");

        // The session audits PROJ-1, then reads PROJ-2 for context — the
        // marker now points at PROJ-2's run. Recording PROJ-1's verdict must
        // not finish PROJ-2's run or delete its marker.
        register_session(&store, markers_dir.path(), &env, "audit", "PROJ-1")
            .unwrap()
            .unwrap();
        let b_id = register_session(&store, markers_dir.path(), &env, "audit", "PROJ-2")
            .unwrap()
            .unwrap();

        let finished = finish_session(
            &store,
            markers_dir.path(),
            &env,
            "audit",
            "PROJ-1",
            RunStatus::Done,
        )
        .unwrap();

        assert_eq!(finished, None);
        assert_eq!(
            store.run_by_id(b_id).unwrap().unwrap().status,
            RunStatus::Running
        );
        assert!(markers_dir.path().join("sess-1").exists());
    }

    #[test]
    fn finish_session_ignores_a_marker_run_of_a_different_kind() {
        let db_dir = tempdir().unwrap();
        let markers_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());
        let env = env_with_session("sess-1");

        let id = register_session(&store, markers_dir.path(), &env, "create", "PROJ-1")
            .unwrap()
            .unwrap();

        let finished = finish_session(
            &store,
            markers_dir.path(),
            &env,
            "audit",
            "PROJ-1",
            RunStatus::Done,
        )
        .unwrap();

        assert_eq!(finished, None);
        assert_eq!(
            store.run_by_id(id).unwrap().unwrap().status,
            RunStatus::Running
        );
    }

    #[test]
    fn finish_session_is_a_noop_without_a_marker() {
        let db_dir = tempdir().unwrap();
        let markers_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());
        let env = env_with_session("sess-nomarker");

        let result = finish_session(
            &store,
            markers_dir.path(),
            &env,
            "audit",
            "PROJ-1",
            RunStatus::Done,
        )
        .unwrap();

        assert_eq!(result, None);
    }

    #[test]
    fn finish_session_is_a_noop_without_a_session_id() {
        let db_dir = tempdir().unwrap();
        let markers_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());
        let env = SessionEnv {
            session_id: None,
            claude_pid: None,
            lane_run_id: None,
            session_run_id: None,
            cwd: PathBuf::from("/tmp/wt"),
        };

        let result = finish_session(
            &store,
            markers_dir.path(),
            &env,
            "audit",
            "PROJ-1",
            RunStatus::Done,
        )
        .unwrap();

        assert_eq!(result, None);
    }

    #[test]
    fn finish_session_is_a_noop_when_a_lane_run_owns_telemetry() {
        let db_dir = tempdir().unwrap();
        let markers_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());
        let env = env_with_session("sess-1");

        register_session(&store, markers_dir.path(), &env, "audit", "PROJ-1")
            .unwrap()
            .unwrap();

        let mut lane_env = env.clone();
        lane_env.lane_run_id = Some("77".to_string());

        let result = finish_session(
            &store,
            markers_dir.path(),
            &lane_env,
            "audit",
            "PROJ-1",
            RunStatus::Done,
        )
        .unwrap();

        assert_eq!(result, None);
        assert!(markers_dir.path().join("sess-1").exists());
    }

    #[test]
    fn sessions_dir_uses_xdg_data_home_when_set() {
        let home = Path::new("/home/user");
        let xdg = Path::new("/custom/data");

        let path = sessions_dir(home, Some(xdg));

        assert_eq!(path, PathBuf::from("/custom/data/tskmstr/sessions"));
    }

    #[test]
    fn sessions_dir_falls_back_to_home_local_share() {
        let home = Path::new("/home/user");

        let path = sessions_dir(home, None);

        assert_eq!(
            path,
            PathBuf::from("/home/user/.local/share/tskmstr/sessions")
        );
    }
}
