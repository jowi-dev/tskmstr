//! SQLite-backed run store for lane execution state.
//!
//! `tskmstr` does not spawn or supervise processes (see
//! `docs/decisions/0001-run-state.md`). This module only persists what a
//! runner and its hooks report: run rows and their events. Ticket data
//! stays in Jira; this store never mirrors it.
//!
//! All timestamps are written by SQLite itself (see [`NOW_SQL`]) rather
//! than a Rust time crate, so every row's `started_at`/`ended_at`/etc. and
//! every computed age is consistent with the database's own clock.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};
use thiserror::Error;

/// A SQL expression yielding the current UTC time as
/// `YYYY-MM-DDTHH:MM:SS.sssZ`. Takes no user input, so it is safe to splice
/// directly into statement text; all user-supplied values still go through
/// bound `?` parameters.
const NOW_SQL: &str = "strftime('%Y-%m-%dT%H:%M:%fZ','now')";

/// Schema migrations, indexed by `PRAGMA user_version`. `MIGRATIONS[0]` is
/// applied to take a fresh database from version 0 to version 1, and so on.
/// Future schema changes append here rather than editing existing entries.
const MIGRATIONS: &[&str] = &[r#"
    CREATE TABLE runs (
      id           INTEGER PRIMARY KEY,
      ticket       TEXT    NOT NULL,
      lane         TEXT    NOT NULL,
      status       TEXT    NOT NULL,
      session_id   TEXT,
      worktree     TEXT    NOT NULL,
      branch       TEXT,
      pid          INTEGER,
      transcript   TEXT,
      started_at   TEXT    NOT NULL,
      heartbeat_at TEXT,
      ended_at     TEXT,
      exit_code    INTEGER,
      num_turns    INTEGER,
      cost_usd     REAL,
      blocker      TEXT,
      pr_url       TEXT
    );
    CREATE TABLE run_events (
      id      INTEGER PRIMARY KEY,
      run_id  INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
      at      TEXT    NOT NULL,
      kind    TEXT    NOT NULL,
      detail  TEXT
    );
    CREATE INDEX idx_events_run ON run_events(run_id, at);
    CREATE INDEX idx_runs_status ON runs(status);
    "#];

/// A handle to the run-state SQLite database.
///
/// Opened once per process via [`RunStore::open`]; safe to hold for the
/// lifetime of a command since PRAGMAs are set to tolerate concurrent
/// writers (WAL journal mode, a 5s busy timeout).
pub struct RunStore {
    conn: Connection,
}

/// Errors returned by [`RunStore`] operations.
#[derive(Debug, Error)]
pub enum RunStoreError {
    /// The parent directory of the database file could not be created.
    #[error("failed to create run db directory {path}: {source}")]
    CreateDir {
        /// Directory that could not be created.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The database file could not be opened.
    #[error("failed to open run db {path}: {source}")]
    Open {
        /// Path that could not be opened.
        path: PathBuf,
        /// Underlying rusqlite error.
        #[source]
        source: rusqlite::Error,
    },

    /// A SQLite operation failed.
    #[error("run db error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// [`RunStore::finish_run`] was called with an id that has no matching row.
    #[error("no run with id {0}")]
    RunNotFound(i64),
}

/// Lifecycle status of a run, stored in `runs.status` as its lowercase name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    /// Queued to run but not yet started.
    Queued,
    /// Currently executing.
    Running,
    /// Waiting on an external dependency or escalation.
    Blocked,
    /// Finished and awaiting human review.
    Review,
    /// Finished successfully.
    Done,
    /// Finished with an error.
    Failed,
}

impl RunStatus {
    /// Returns the lowercase string stored in the database for this status.
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Queued => "queued",
            RunStatus::Running => "running",
            RunStatus::Blocked => "blocked",
            RunStatus::Review => "review",
            RunStatus::Done => "done",
            RunStatus::Failed => "failed",
        }
    }

    /// Parses a status string as stored in the database. Returns `None` for
    /// unrecognized values.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "queued" => Some(RunStatus::Queued),
            "running" => Some(RunStatus::Running),
            "blocked" => Some(RunStatus::Blocked),
            "review" => Some(RunStatus::Review),
            "done" => Some(RunStatus::Done),
            "failed" => Some(RunStatus::Failed),
            _ => None,
        }
    }
}

/// Parameters for starting a new run.
#[derive(Debug, Clone)]
pub struct StartRun {
    /// Jira ticket key the run is working, e.g. `PROJ-123`.
    pub ticket: String,
    /// Lane name the run executed in.
    pub lane: String,
    /// Filesystem path of the git worktree the run used.
    pub worktree: String,
    /// Branch checked out in the worktree, if known.
    pub branch: Option<String>,
    /// PID of the runner process, if known.
    pub pid: Option<u32>,
}

/// Outcome fields recorded when a run finishes.
///
/// Every field besides `status` is optional; a `None` leaves the existing
/// column value untouched rather than clearing it.
#[derive(Debug, Clone)]
pub struct FinishRun {
    /// Final status of the run.
    pub status: RunStatus,
    /// Process exit code, if the run exited normally.
    pub exit_code: Option<i32>,
    /// `claude -p` session id, enabling `claude --resume`.
    pub session_id: Option<String>,
    /// Reported cost of the run in USD.
    pub cost_usd: Option<f64>,
    /// Number of turns the run took.
    pub num_turns: Option<i64>,
    /// Escalation text, set when `status` is [`RunStatus::Blocked`].
    pub blocker: Option<String>,
    /// URL of the pull request the run opened, if any.
    pub pr_url: Option<String>,
    /// Filesystem path of the full transcript, if one was captured.
    pub transcript: Option<String>,
}

impl Default for FinishRun {
    /// Defaults to [`RunStatus::Failed`] with no other fields set. There is
    /// no meaningful "default" run outcome, but a run that gets finished
    /// with an unpopulated outcome should read as a failure rather than
    /// silently ending up in a state ([`RunStatus::Queued`], say) implying
    /// it never ran.
    fn default() -> Self {
        FinishRun {
            status: RunStatus::Failed,
            exit_code: None,
            session_id: None,
            cost_usd: None,
            num_turns: None,
            blocker: None,
            pr_url: None,
            transcript: None,
        }
    }
}

/// A single row from [`RunStore::list_runs`], with ages precomputed in SQL.
#[derive(Debug, Clone)]
pub struct RunSummary {
    /// Row id.
    pub id: i64,
    /// Jira ticket key.
    pub ticket: String,
    /// Lane name.
    pub lane: String,
    /// Current status.
    pub status: RunStatus,
    /// Seconds since `started_at`.
    pub age_secs: i64,
    /// Seconds since the last heartbeat (or `started_at` if none), or
    /// `None` if the run has already ended.
    pub heartbeat_age_secs: Option<i64>,
    /// `kind` of the most recent `run_events` row for this run, if any.
    pub last_event_kind: Option<String>,
    /// Seconds since the most recent event, if any.
    pub last_event_age_secs: Option<i64>,
}

impl RunStore {
    /// Opens (creating if necessary) the run database at `path`, applying
    /// any pending migrations.
    ///
    /// # Errors
    ///
    /// Returns [`RunStoreError::CreateDir`] if `path`'s parent directory
    /// does not exist and cannot be created, or [`RunStoreError::Open`] if
    /// the database file itself cannot be opened.
    pub fn open(path: &Path) -> Result<Self, RunStoreError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|source| RunStoreError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let conn = Connection::open(path).map_err(|source| RunStoreError::Open {
            path: path.to_path_buf(),
            source,
        })?;

        conn.query_row("PRAGMA journal_mode = WAL", [], |row| {
            row.get::<_, String>(0)
        })?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;

        let store = RunStore { conn };
        store.migrate()?;
        Ok(store)
    }

    /// Applies any migrations in [`MIGRATIONS`] not yet reflected in
    /// `PRAGMA user_version`.
    fn migrate(&self) -> Result<(), RunStoreError> {
        let current: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        let current = current as usize;

        for (i, migration) in MIGRATIONS.iter().enumerate().skip(current) {
            let version = i + 1;
            self.conn.execute_batch(migration)?;
            self.conn
                .execute_batch(&format!("PRAGMA user_version = {version}"))?;
        }

        Ok(())
    }

    /// Inserts a new run row with status `running` and `started_at` set to
    /// the database's current time, returning the new row id.
    pub fn start_run(&self, params: &StartRun) -> Result<i64, RunStoreError> {
        self.conn.execute(
            &format!(
                "INSERT INTO runs (ticket, lane, status, worktree, branch, pid, started_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, {NOW_SQL})"
            ),
            params![
                params.ticket,
                params.lane,
                RunStatus::Running.as_str(),
                params.worktree,
                params.branch,
                params.pid,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Records the outcome of a finished run.
    ///
    /// `outcome.status` always overwrites the row's status; every other
    /// field is only written when `Some`, so partial outcomes (e.g. just a
    /// blocker message) don't clobber previously recorded values.
    ///
    /// # Errors
    ///
    /// Returns [`RunStoreError::RunNotFound`] if `run_id` has no matching row.
    pub fn finish_run(&self, run_id: i64, outcome: &FinishRun) -> Result<(), RunStoreError> {
        let changes = self.conn.execute(
            &format!(
                "UPDATE runs SET
                    status = ?1,
                    ended_at = {NOW_SQL},
                    exit_code = COALESCE(?2, exit_code),
                    session_id = COALESCE(?3, session_id),
                    cost_usd = COALESCE(?4, cost_usd),
                    num_turns = COALESCE(?5, num_turns),
                    blocker = COALESCE(?6, blocker),
                    pr_url = COALESCE(?7, pr_url),
                    transcript = COALESCE(?8, transcript)
                 WHERE id = ?9"
            ),
            params![
                outcome.status.as_str(),
                outcome.exit_code,
                outcome.session_id,
                outcome.cost_usd,
                outcome.num_turns,
                outcome.blocker,
                outcome.pr_url,
                outcome.transcript,
                run_id,
            ],
        )?;

        if changes == 0 {
            return Err(RunStoreError::RunNotFound(run_id));
        }
        Ok(())
    }

    /// Appends an event to a run and bumps the run's heartbeat, atomically.
    ///
    /// `detail` is stored as-is; validating it (e.g. as JSON) is the CLI
    /// layer's responsibility. Returns the new event row's id.
    ///
    /// # Errors
    ///
    /// Returns [`RunStoreError::RunNotFound`] if `run_id` has no matching
    /// row; no event row is inserted in that case.
    pub fn add_event(
        &self,
        run_id: i64,
        kind: &str,
        detail: Option<&str>,
    ) -> Result<i64, RunStoreError> {
        let tx = self.conn.unchecked_transaction()?;

        let changes = tx.execute(
            &format!("UPDATE runs SET heartbeat_at = {NOW_SQL} WHERE id = ?1"),
            params![run_id],
        )?;
        if changes == 0 {
            return Err(RunStoreError::RunNotFound(run_id));
        }

        tx.execute(
            &format!(
                "INSERT INTO run_events (run_id, at, kind, detail) VALUES (?1, {NOW_SQL}, ?2, ?3)"
            ),
            params![run_id, kind, detail],
        )?;
        let event_id = tx.last_insert_rowid();

        tx.commit()?;
        Ok(event_id)
    }

    /// Lists all runs, ordered with active runs (queued/running/blocked/
    /// review) before terminal ones (done/failed), and by `started_at`
    /// descending within each group.
    pub fn list_runs(&self) -> Result<Vec<RunSummary>, RunStoreError> {
        let sql = "SELECT
                r.id,
                r.ticket,
                r.lane,
                r.status,
                CAST((julianday('now') - julianday(r.started_at)) * 86400 AS INTEGER) AS age_secs,
                CASE WHEN r.ended_at IS NULL THEN
                    CAST((julianday('now') - julianday(COALESCE(r.heartbeat_at, r.started_at))) * 86400 AS INTEGER)
                ELSE NULL END AS heartbeat_age_secs,
                (SELECT e.kind FROM run_events e WHERE e.run_id = r.id ORDER BY e.at DESC, e.id DESC LIMIT 1) AS last_event_kind,
                (SELECT CAST((julianday('now') - julianday(e.at)) * 86400 AS INTEGER)
                    FROM run_events e WHERE e.run_id = r.id ORDER BY e.at DESC, e.id DESC LIMIT 1) AS last_event_age_secs
             FROM runs r
             ORDER BY
                CASE r.status WHEN 'done' THEN 1 WHEN 'failed' THEN 1 ELSE 0 END ASC,
                r.started_at DESC";

        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map([], |row| {
            let status_str: String = row.get(3)?;
            let status = RunStatus::parse(&status_str).unwrap_or(RunStatus::Failed);
            Ok(RunSummary {
                id: row.get(0)?,
                ticket: row.get(1)?,
                lane: row.get(2)?,
                status,
                age_secs: row.get(4)?,
                heartbeat_age_secs: row.get(5)?,
                last_event_kind: row.get(6)?,
                last_event_age_secs: row.get(7)?,
            })
        })?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

/// Returns the default path for the run database: `$XDG_DATA_HOME/tskmstr/runs.db`
/// when `xdg_data_home` is set, otherwise `home/.local/share/tskmstr/runs.db`.
pub fn default_db_path(home: &Path, xdg_data_home: Option<&Path>) -> PathBuf {
    match xdg_data_home {
        Some(xdg) => xdg.join("tskmstr").join("runs.db"),
        None => home
            .join(".local")
            .join("share")
            .join("tskmstr")
            .join("runs.db"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;
    use tempfile::tempdir;

    fn open_store(dir: &Path) -> RunStore {
        RunStore::open(&dir.join("nested").join("runs.db")).expect("open should succeed")
    }

    #[test]
    fn open_creates_file_and_parent_dirs() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("nested").join("deep").join("runs.db");
        assert!(!db_path.exists());

        RunStore::open(&db_path).expect("open should succeed");

        assert!(db_path.exists());
    }

    #[test]
    fn open_sets_expected_pragmas() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let journal_mode: String = store
            .conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal_mode, "wal");

        let busy_timeout: i64 = store
            .conn
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();
        assert_eq!(busy_timeout, 5000);

        let foreign_keys: i64 = store
            .conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .unwrap();
        assert_eq!(foreign_keys, 1);
    }

    #[test]
    fn reopen_of_existing_db_is_idempotent() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("runs.db");

        {
            let store = RunStore::open(&db_path).unwrap();
            let version: i64 = store
                .conn
                .query_row("PRAGMA user_version", [], |row| row.get(0))
                .unwrap();
            assert_eq!(version, 1);
        }

        let store = RunStore::open(&db_path).expect("reopen should succeed");
        let version: i64 = store
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 1);
    }

    #[test]
    fn start_run_returns_incrementing_ids_and_round_trips() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let id1 = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "backend".to_string(),
                worktree: "/tmp/wt1".to_string(),
                branch: Some("proj-1".to_string()),
                pid: Some(1234),
            })
            .unwrap();
        let id2 = store
            .start_run(&StartRun {
                ticket: "PROJ-2".to_string(),
                lane: "backend".to_string(),
                worktree: "/tmp/wt2".to_string(),
                branch: None,
                pid: None,
            })
            .unwrap();

        assert_eq!(id1, 1);
        assert_eq!(id2, 2);

        let (status, started_at): (String, String) = store
            .conn
            .query_row(
                "SELECT status, started_at FROM runs WHERE id = ?1",
                params![id1],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();

        assert_eq!(status, "running");
        assert!(!started_at.is_empty());
        let re = Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$").unwrap();
        assert!(
            re.is_match(&started_at),
            "started_at {started_at:?} did not match expected format"
        );
    }

    #[test]
    fn finish_run_sets_status_ended_at_and_optionals() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "backend".to_string(),
                worktree: "/tmp/wt1".to_string(),
                branch: None,
                pid: None,
            })
            .unwrap();

        store
            .finish_run(
                id,
                &FinishRun {
                    status: RunStatus::Done,
                    exit_code: Some(0),
                    session_id: Some("sess-abc".to_string()),
                    cost_usd: Some(1.23),
                    num_turns: Some(7),
                    blocker: None,
                    pr_url: Some("https://example.invalid/pr/1".to_string()),
                    transcript: Some("/tmp/transcript.log".to_string()),
                },
            )
            .unwrap();

        type FinishedRunRow = (
            String,
            Option<String>,
            Option<i32>,
            Option<String>,
            Option<f64>,
            Option<i64>,
            Option<String>,
            Option<String>,
        );

        let (status, ended_at, exit_code, session_id, cost_usd, num_turns, pr_url, transcript): FinishedRunRow = store
            .conn
            .query_row(
                "SELECT status, ended_at, exit_code, session_id, cost_usd, num_turns, pr_url, transcript
                 FROM runs WHERE id = ?1",
                params![id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                    ))
                },
            )
            .unwrap();

        assert_eq!(status, "done");
        assert!(ended_at.is_some());
        assert_eq!(exit_code, Some(0));
        assert_eq!(session_id, Some("sess-abc".to_string()));
        assert_eq!(cost_usd, Some(1.23));
        assert_eq!(num_turns, Some(7));
        assert_eq!(pr_url, Some("https://example.invalid/pr/1".to_string()));
        assert_eq!(transcript, Some("/tmp/transcript.log".to_string()));
    }

    #[test]
    fn finish_run_unknown_id_returns_run_not_found() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let err = store
            .finish_run(999, &FinishRun::default())
            .expect_err("expected RunNotFound");

        match err {
            RunStoreError::RunNotFound(id) => assert_eq!(id, 999),
            other => panic!("expected RunNotFound, got {other:?}"),
        }
    }

    #[test]
    fn list_runs_orders_active_before_terminal_then_by_started_at_desc() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let done_id = store
            .start_run(&StartRun {
                ticket: "PROJ-DONE".to_string(),
                lane: "backend".to_string(),
                worktree: "/tmp/wt-done".to_string(),
                branch: None,
                pid: None,
            })
            .unwrap();
        store
            .finish_run(
                done_id,
                &FinishRun {
                    status: RunStatus::Done,
                    ..FinishRun::default()
                },
            )
            .unwrap();

        // Backdate the done run so it would otherwise sort first by started_at.
        store
            .conn
            .execute(
                "UPDATE runs SET started_at = '2000-01-01T00:00:00.000Z' WHERE id = ?1",
                params![done_id],
            )
            .unwrap();

        let running_older_id = store
            .start_run(&StartRun {
                ticket: "PROJ-OLDER".to_string(),
                lane: "backend".to_string(),
                worktree: "/tmp/wt-older".to_string(),
                branch: None,
                pid: None,
            })
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE runs SET started_at = '2020-01-01T00:00:00.000Z' WHERE id = ?1",
                params![running_older_id],
            )
            .unwrap();

        let running_newer_id = store
            .start_run(&StartRun {
                ticket: "PROJ-NEWER".to_string(),
                lane: "backend".to_string(),
                worktree: "/tmp/wt-newer".to_string(),
                branch: None,
                pid: None,
            })
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE runs SET started_at = '2020-06-01T00:00:00.000Z' WHERE id = ?1",
                params![running_newer_id],
            )
            .unwrap();

        let runs = store.list_runs().unwrap();
        let ids: Vec<i64> = runs.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![running_newer_id, running_older_id, done_id]);

        for run in &runs {
            assert!(run.age_secs >= 0, "age_secs should be non-negative");
        }

        let running_newer = runs.iter().find(|r| r.id == running_newer_id).unwrap();
        assert!(running_newer.last_event_kind.is_none());
        assert!(running_newer.last_event_age_secs.is_none());
        assert!(running_newer.heartbeat_age_secs.is_some());

        let done = runs.iter().find(|r| r.id == done_id).unwrap();
        assert!(done.heartbeat_age_secs.is_none());
    }

    #[test]
    fn list_runs_reports_last_event() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let id = store
            .start_run(&StartRun {
                ticket: "PROJ-EVT".to_string(),
                lane: "backend".to_string(),
                worktree: "/tmp/wt-evt".to_string(),
                branch: None,
                pid: None,
            })
            .unwrap();

        store
            .conn
            .execute(
                &format!(
                    "INSERT INTO run_events (run_id, at, kind, detail) VALUES (?1, {NOW_SQL}, 'tool_use', NULL)"
                ),
                params![id],
            )
            .unwrap();

        let runs = store.list_runs().unwrap();
        let run = runs.iter().find(|r| r.id == id).unwrap();
        assert_eq!(run.last_event_kind.as_deref(), Some("tool_use"));
        assert!(run.last_event_age_secs.is_some());
        assert!(run.last_event_age_secs.unwrap() >= 0);
    }

    #[test]
    fn add_event_writes_row_and_bumps_heartbeat() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let run_id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "backend".to_string(),
                worktree: "/tmp/wt1".to_string(),
                branch: None,
                pid: None,
            })
            .unwrap();

        let event_id = store
            .add_event(run_id, "tool_use", Some(r#"{"file":"a.rs"}"#))
            .unwrap();
        assert_eq!(event_id, 1);

        let (kind, detail, at): (String, Option<String>, String) = store
            .conn
            .query_row(
                "SELECT kind, detail, at FROM run_events WHERE id = ?1",
                params![event_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();

        assert_eq!(kind, "tool_use");
        assert_eq!(detail, Some(r#"{"file":"a.rs"}"#.to_string()));
        let re = Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$").unwrap();
        assert!(re.is_match(&at), "at {at:?} did not match expected format");

        let heartbeat_at: Option<String> = store
            .conn
            .query_row(
                "SELECT heartbeat_at FROM runs WHERE id = ?1",
                params![run_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(heartbeat_at.is_some());
    }

    #[test]
    fn add_event_orders_two_events_by_at() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let run_id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "backend".to_string(),
                worktree: "/tmp/wt1".to_string(),
                branch: None,
                pid: None,
            })
            .unwrap();

        let first_id = store.add_event(run_id, "first", None).unwrap();
        store
            .conn
            .execute(
                "UPDATE run_events SET at = '2000-01-01T00:00:00.000Z' WHERE id = ?1",
                params![first_id],
            )
            .unwrap();
        let second_id = store.add_event(run_id, "second", None).unwrap();

        let mut stmt = store
            .conn
            .prepare("SELECT id FROM run_events WHERE run_id = ?1 ORDER BY at ASC")
            .unwrap();
        let ids: Vec<i64> = stmt
            .query_map(params![run_id], |row| row.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        assert_eq!(ids, vec![first_id, second_id]);
    }

    #[test]
    fn add_event_unknown_run_id_returns_run_not_found_and_inserts_nothing() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let err = store
            .add_event(999, "tool_use", None)
            .expect_err("expected RunNotFound");

        match err {
            RunStoreError::RunNotFound(id) => assert_eq!(id, 999),
            other => panic!("expected RunNotFound, got {other:?}"),
        }

        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM run_events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn add_event_is_safe_under_concurrent_writers() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("runs.db");

        let run_id = {
            let store = RunStore::open(&db_path).unwrap();
            store
                .start_run(&StartRun {
                    ticket: "PROJ-1".to_string(),
                    lane: "backend".to_string(),
                    worktree: "/tmp/wt1".to_string(),
                    branch: None,
                    pid: None,
                })
                .unwrap()
        };

        let mut handles = Vec::new();
        for _ in 0..2 {
            let db_path = db_path.clone();
            handles.push(std::thread::spawn(move || {
                let store = RunStore::open(&db_path).expect("open should succeed");
                for i in 0..250 {
                    store
                        .add_event(run_id, "tool_use", Some(&i.to_string()))
                        .expect("add_event should succeed");
                }
            }));
        }

        for handle in handles {
            handle.join().expect("thread should not panic");
        }

        let store = RunStore::open(&db_path).unwrap();
        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM run_events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 500);
    }

    #[test]
    fn default_db_path_uses_xdg_data_home_when_set() {
        let home = Path::new("/home/user");
        let xdg = Path::new("/custom/data");

        let path = default_db_path(home, Some(xdg));

        assert_eq!(path, PathBuf::from("/custom/data/tskmstr/runs.db"));
    }

    #[test]
    fn default_db_path_falls_back_to_home_local_share() {
        let home = Path::new("/home/user");

        let path = default_db_path(home, None);

        assert_eq!(
            path,
            PathBuf::from("/home/user/.local/share/tskmstr/runs.db")
        );
    }
}
