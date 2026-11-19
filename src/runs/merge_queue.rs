//! The merge queue (GitHub issue #247): per scope, an ordered list of
//! tickets waiting to merge, drained by the automerge sweep
//! ([`crate::work::auto_merge::sweep`]).
//!
//! It lives in runs.db, not TUI memory, so it outlives the TUI and the
//! dispatch tick (which runs without one) can read it. Each row records
//! how it got there ([`QueueSource`]) and, once a merge has been tried,
//! the PR head it was tried on. A merge that does not land leaves its row
//! *held* until the PR's head changes.

use rusqlite::{OptionalExtension, params};

use super::{NOW_SQL, RunStore, RunStoreError};

/// How a ticket got into the merge queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueSource {
    /// The operator added it: `tm merge --queue`, or the TUI.
    Manual,
    /// The automerge sweep added it when its review finished.
    Autopilot,
}

impl QueueSource {
    /// The value stored in `merge_queue.source`.
    pub fn as_str(self) -> &'static str {
        match self {
            QueueSource::Manual => "manual",
            QueueSource::Autopilot => "autopilot",
        }
    }

    fn parse(s: &str) -> Self {
        if s == "autopilot" {
            QueueSource::Autopilot
        } else {
            QueueSource::Manual
        }
    }
}

/// One `merge_queue` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeQueueEntry {
    /// The scope ([`crate::config::BackendIdentity::scope`]).
    pub scope: String,
    /// Ticket key.
    pub ticket: String,
    /// How it got into the queue.
    pub source: QueueSource,
    /// The PR head commit the last merge attempt started on, if any.
    pub head_sha: Option<String>,
    /// Why the last attempt did not land. `Some` means the row is held:
    /// the sweep skips it while the PR's head is still
    /// [`MergeQueueEntry::head_sha`].
    pub held: Option<String>,
    /// Why the last sweep passed over the row (`"checks pending"`), for
    /// display. `None` before any sweep looked at it and once a merge
    /// starts.
    pub waiting: Option<String>,
    /// When it was added, as [`NOW_SQL`] renders it.
    pub added_at: String,
}

impl MergeQueueEntry {
    /// Whether the sweep must skip this row while the PR's head is `sha`.
    pub fn held_at(&self, sha: &str) -> bool {
        self.held.is_some() && self.head_sha.as_deref() == Some(sha)
    }
}

impl RunStore {
    /// Appends `ticket` to the end of `scope`'s merge queue. Returns
    /// `false`, changing nothing, when it is already queued.
    pub fn enqueue_merge(
        &self,
        scope: &str,
        ticket: &str,
        source: QueueSource,
    ) -> Result<bool, RunStoreError> {
        let added = self.conn.execute(
            &format!(
                "INSERT INTO merge_queue (scope, ticket, position, source, added_at)
                 VALUES (?1, ?2,
                         (SELECT COALESCE(MAX(position), 0) + 1
                            FROM merge_queue WHERE scope = ?1),
                         ?3, {NOW_SQL})
                 ON CONFLICT(scope, ticket) DO NOTHING"
            ),
            params![scope, ticket, source.as_str()],
        )?;
        Ok(added == 1)
    }

    /// Removes `ticket` from `scope`'s merge queue. Returns whether it was
    /// queued.
    pub fn dequeue_merge(&self, scope: &str, ticket: &str) -> Result<bool, RunStoreError> {
        let removed = self.conn.execute(
            "DELETE FROM merge_queue WHERE scope = ?1 AND ticket = ?2",
            params![scope, ticket],
        )?;
        Ok(removed == 1)
    }

    /// `scope`'s merge queue, first to merge first.
    pub fn merge_queue(&self, scope: &str) -> Result<Vec<MergeQueueEntry>, RunStoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT scope, ticket, source, head_sha, held, waiting, added_at FROM merge_queue
              WHERE scope = ?1 ORDER BY position, ticket",
        )?;
        let rows = stmt.query_map(params![scope], |row| {
            Ok(MergeQueueEntry {
                scope: row.get(0)?,
                ticket: row.get(1)?,
                source: QueueSource::parse(&row.get::<_, String>(2)?),
                head_sha: row.get(3)?,
                held: row.get(4)?,
                waiting: row.get(5)?,
                added_at: row.get(6)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(RunStoreError::from)
    }

    /// Every scope with at least one queued merge, by name.
    pub fn merge_queue_scopes(&self) -> Result<Vec<String>, RunStoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT scope FROM merge_queue ORDER BY scope")?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(RunStoreError::from)
    }

    /// Moves `ticket` to `index` (0 = first to merge) in `scope`'s queue,
    /// clamped to the queue's end. Returns `false`, changing nothing, when
    /// it is not queued.
    pub fn move_merge(
        &self,
        scope: &str,
        ticket: &str,
        index: usize,
    ) -> Result<bool, RunStoreError> {
        let mut order: Vec<String> = self
            .merge_queue(scope)?
            .into_iter()
            .map(|entry| entry.ticket)
            .collect();
        let Some(from) = order.iter().position(|t| t == ticket) else {
            return Ok(false);
        };
        let moved = order.remove(from);
        order.insert(index.min(order.len()), moved);
        let tx = self.conn.unchecked_transaction()?;
        for (position, ticket) in order.iter().enumerate() {
            tx.execute(
                "UPDATE merge_queue SET position = ?3 WHERE scope = ?1 AND ticket = ?2",
                params![scope, ticket, position as i64 + 1],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// Records that a merge of `ticket` started on PR head `sha`, clearing
    /// any hold.
    pub fn mark_merge_started(
        &self,
        scope: &str,
        ticket: &str,
        sha: &str,
    ) -> Result<(), RunStoreError> {
        self.conn.execute(
            "UPDATE merge_queue SET head_sha = ?3, held = NULL, waiting = NULL
              WHERE scope = ?1 AND ticket = ?2",
            params![scope, ticket, sha],
        )?;
        Ok(())
    }

    /// Holds `ticket`'s row with `reason`: its merge did not land, and the
    /// sweep skips it until the PR's head changes.
    pub fn hold_merge(&self, scope: &str, ticket: &str, reason: &str) -> Result<(), RunStoreError> {
        self.conn.execute(
            "UPDATE merge_queue SET held = ?3 WHERE scope = ?1 AND ticket = ?2",
            params![scope, ticket, reason],
        )?;
        Ok(())
    }

    /// Records why the sweep passed over `ticket`'s row this time, or with
    /// `None` that nothing held it back.
    pub fn set_merge_waiting(
        &self,
        scope: &str,
        ticket: &str,
        reason: Option<&str>,
    ) -> Result<(), RunStoreError> {
        self.conn.execute(
            "UPDATE merge_queue SET waiting = ?3 WHERE scope = ?1 AND ticket = ?2",
            params![scope, ticket, reason],
        )?;
        Ok(())
    }

    /// Records `repo_dir` as `scope`'s main checkout without touching its
    /// autopilot flag (a new row is off), so `tm dispatch tick --all` can
    /// drain the scope's queue from any directory.
    pub fn record_autopilot_repo(&self, scope: &str, repo_dir: &str) -> Result<(), RunStoreError> {
        self.conn.execute(
            &format!(
                "INSERT INTO autopilot (scope, enabled, updated_at, repo_dir)
                 VALUES (?1, 0, {NOW_SQL}, ?2)
                 ON CONFLICT(scope) DO UPDATE SET repo_dir = excluded.repo_dir"
            ),
            params![scope, repo_dir],
        )?;
        Ok(())
    }

    /// Sets or, with `None`, clears `scope`'s merge epic: when set, the
    /// automerge sweep enqueues only that epic's tickets on its own. Never
    /// changes the scope's autopilot flag.
    pub fn set_merge_epic(&self, scope: &str, epic: Option<&str>) -> Result<(), RunStoreError> {
        self.conn.execute(
            &format!(
                "INSERT INTO autopilot (scope, enabled, updated_at, merge_epic)
                 VALUES (?1, 0, {NOW_SQL}, ?2)
                 ON CONFLICT(scope) DO UPDATE SET merge_epic = excluded.merge_epic"
            ),
            params![scope, epic],
        )?;
        Ok(())
    }

    /// `scope`'s merge epic, or `None` when none is set.
    pub fn merge_epic(&self, scope: &str) -> Result<Option<String>, RunStoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT merge_epic FROM autopilot WHERE scope = ?1",
                params![scope],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::{TempDir, tempdir};

    const SCOPE: &str = "github:o/r";

    fn store() -> (TempDir, RunStore) {
        let dir = tempdir().unwrap();
        let store = RunStore::open(&dir.path().join("runs.db")).unwrap();
        (dir, store)
    }

    fn tickets(store: &RunStore, scope: &str) -> Vec<String> {
        store
            .merge_queue(scope)
            .unwrap()
            .into_iter()
            .map(|e| e.ticket)
            .collect()
    }

    #[test]
    fn an_empty_scope_has_an_empty_queue() {
        let (_dir, store) = store();
        assert!(store.merge_queue(SCOPE).unwrap().is_empty());
        assert!(store.merge_queue_scopes().unwrap().is_empty());
    }

    #[test]
    fn enqueue_appends_in_order_and_records_the_source() {
        let (_dir, store) = store();
        assert!(
            store
                .enqueue_merge(SCOPE, "GH-2", QueueSource::Manual)
                .unwrap()
        );
        assert!(
            store
                .enqueue_merge(SCOPE, "GH-1", QueueSource::Autopilot)
                .unwrap()
        );

        let queue = store.merge_queue(SCOPE).unwrap();
        assert_eq!(tickets(&store, SCOPE), vec!["GH-2", "GH-1"]);
        assert_eq!(queue[0].source, QueueSource::Manual);
        assert_eq!(queue[1].source, QueueSource::Autopilot);
        assert_eq!(queue[0].held, None);
        assert_eq!(queue[0].head_sha, None);
    }

    #[test]
    fn enqueueing_a_queued_ticket_changes_nothing() {
        let (_dir, store) = store();
        store
            .enqueue_merge(SCOPE, "GH-1", QueueSource::Manual)
            .unwrap();
        store
            .enqueue_merge(SCOPE, "GH-2", QueueSource::Manual)
            .unwrap();

        assert!(
            !store
                .enqueue_merge(SCOPE, "GH-1", QueueSource::Autopilot)
                .unwrap()
        );

        let queue = store.merge_queue(SCOPE).unwrap();
        assert_eq!(tickets(&store, SCOPE), vec!["GH-1", "GH-2"]);
        assert_eq!(queue[0].source, QueueSource::Manual);
    }

    #[test]
    fn queues_are_per_scope() {
        let (_dir, store) = store();
        store
            .enqueue_merge(SCOPE, "GH-1", QueueSource::Manual)
            .unwrap();
        store
            .enqueue_merge("github:o/other", "GH-1", QueueSource::Manual)
            .unwrap();

        assert!(store.dequeue_merge("github:o/other", "GH-1").unwrap());

        assert_eq!(tickets(&store, SCOPE), vec!["GH-1"]);
        assert_eq!(store.merge_queue_scopes().unwrap(), vec![SCOPE]);
    }

    #[test]
    fn dequeue_reports_whether_the_ticket_was_queued() {
        let (_dir, store) = store();
        store
            .enqueue_merge(SCOPE, "GH-1", QueueSource::Manual)
            .unwrap();

        assert!(store.dequeue_merge(SCOPE, "GH-1").unwrap());
        assert!(!store.dequeue_merge(SCOPE, "GH-1").unwrap());
        assert!(store.merge_queue(SCOPE).unwrap().is_empty());
    }

    #[test]
    fn move_merge_reorders_and_clamps() {
        let (_dir, store) = store();
        for t in ["GH-1", "GH-2", "GH-3"] {
            store.enqueue_merge(SCOPE, t, QueueSource::Manual).unwrap();
        }

        assert!(store.move_merge(SCOPE, "GH-3", 0).unwrap());
        assert_eq!(tickets(&store, SCOPE), vec!["GH-3", "GH-1", "GH-2"]);

        assert!(store.move_merge(SCOPE, "GH-3", 99).unwrap());
        assert_eq!(tickets(&store, SCOPE), vec!["GH-1", "GH-2", "GH-3"]);

        assert!(!store.move_merge(SCOPE, "GH-9", 0).unwrap());

        // A ticket enqueued after a move still lands at the end.
        store
            .enqueue_merge(SCOPE, "GH-4", QueueSource::Manual)
            .unwrap();
        assert_eq!(tickets(&store, SCOPE), vec!["GH-1", "GH-2", "GH-3", "GH-4"]);
    }

    #[test]
    fn a_held_row_is_held_only_at_the_head_it_failed_on() {
        let (_dir, store) = store();
        store
            .enqueue_merge(SCOPE, "GH-1", QueueSource::Manual)
            .unwrap();
        store.mark_merge_started(SCOPE, "GH-1", "abc").unwrap();
        store.hold_merge(SCOPE, "GH-1", "checks failed").unwrap();

        let entry = &store.merge_queue(SCOPE).unwrap()[0];
        assert_eq!(entry.held.as_deref(), Some("checks failed"));
        assert!(entry.held_at("abc"));
        assert!(!entry.held_at("def"));

        // A new attempt clears the hold.
        store.mark_merge_started(SCOPE, "GH-1", "def").unwrap();
        let entry = &store.merge_queue(SCOPE).unwrap()[0];
        assert_eq!(entry.held, None);
        assert_eq!(entry.head_sha.as_deref(), Some("def"));
    }

    #[test]
    fn waiting_is_recorded_and_cleared_when_a_merge_starts() {
        let (_dir, store) = store();
        store
            .enqueue_merge(SCOPE, "GH-1", QueueSource::Manual)
            .unwrap();
        assert_eq!(store.merge_queue(SCOPE).unwrap()[0].waiting, None);

        store
            .set_merge_waiting(SCOPE, "GH-1", Some("checks pending"))
            .unwrap();
        assert_eq!(
            store.merge_queue(SCOPE).unwrap()[0].waiting.as_deref(),
            Some("checks pending")
        );

        store.mark_merge_started(SCOPE, "GH-1", "abc").unwrap();
        assert_eq!(store.merge_queue(SCOPE).unwrap()[0].waiting, None);
    }

    #[test]
    fn record_autopilot_repo_keeps_the_flag() {
        let (_dir, store) = store();
        store.record_autopilot_repo(SCOPE, "/repos/r").unwrap();
        let rows = store.autopilot_scopes().unwrap();
        assert_eq!(rows[0].repo_dir.as_deref(), Some("/repos/r"));
        assert!(!rows[0].enabled);

        store.set_autopilot(SCOPE, true).unwrap();
        store.record_autopilot_repo(SCOPE, "/repos/r2").unwrap();
        let rows = store.autopilot_scopes().unwrap();
        assert_eq!(rows[0].repo_dir.as_deref(), Some("/repos/r2"));
        assert!(rows[0].enabled);
    }

    #[test]
    fn the_queue_survives_reopening_the_store() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("runs.db");
        {
            let store = RunStore::open(&path).unwrap();
            store
                .enqueue_merge(SCOPE, "GH-1", QueueSource::Manual)
                .unwrap();
        }
        let store = RunStore::open(&path).unwrap();
        assert_eq!(tickets(&store, SCOPE), vec!["GH-1"]);
    }

    #[test]
    fn merge_epic_sets_and_clears_without_touching_the_flag_or_priority_epic() {
        let (_dir, store) = store();
        assert_eq!(store.merge_epic(SCOPE).unwrap(), None);

        store.set_autopilot(SCOPE, true).unwrap();
        store.set_autopilot_epic(SCOPE, Some("GH-100")).unwrap();
        store.set_merge_epic(SCOPE, Some("GH-200")).unwrap();

        assert_eq!(store.merge_epic(SCOPE).unwrap().as_deref(), Some("GH-200"));
        assert_eq!(
            store.autopilot_epic(SCOPE).unwrap().as_deref(),
            Some("GH-100")
        );
        assert!(store.autopilot_enabled(SCOPE).unwrap());
        assert_eq!(store.merge_epic("github:o/other").unwrap(), None);

        store.set_merge_epic(SCOPE, None).unwrap();
        assert_eq!(store.merge_epic(SCOPE).unwrap(), None);
        assert_eq!(
            store.autopilot_epic(SCOPE).unwrap().as_deref(),
            Some("GH-100")
        );
    }
}
