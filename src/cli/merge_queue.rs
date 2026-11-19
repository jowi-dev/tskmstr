//! `tm merge --queue` / `--unqueue`, and the merge queue lines of
//! `tm dispatch status` (GitHub issue #247).
//!
//! The queue itself lives in runs.db ([`crate::runs::merge_queue`]); the
//! automerge sweep drains it ([`crate::work::auto_merge`]).

use std::io::Write;

use crate::runs::RunStore;
use crate::runs::merge_queue::QueueSource;
use crate::runs::overview::OverviewRun;
use crate::work::auto_merge::row_state;

use super::dispatch::{CurrentRepo, DispatchCliError};

/// `tm merge --queue KEY...`: appends each key (uppercased) to the current
/// repo's merge queue, and records the repo's checkout so the dispatch
/// timer can drain the queue with autopilot off.
///
/// # Errors
///
/// A run-store or output failure.
pub fn queue(
    store: &RunStore,
    repo: &CurrentRepo,
    keys: &[String],
    out: &mut dyn Write,
) -> Result<(), DispatchCliError> {
    store.record_autopilot_repo(&repo.scope, &repo.repo_dir.display().to_string())?;
    for key in keys {
        let key = key.to_uppercase();
        let added = store.enqueue_merge(&repo.scope, &key, QueueSource::Manual)?;
        let queue = store.merge_queue(&repo.scope)?;
        let position = queue.iter().position(|e| e.ticket == key).unwrap_or(0) + 1;
        if added {
            writeln!(out, "queued {key} to merge ({position} of {})", queue.len())?;
        } else {
            writeln!(
                out,
                "{key} is already queued ({position} of {})",
                queue.len()
            )?;
        }
    }
    Ok(())
}

/// `tm merge --unqueue KEY...`: removes each key from the current repo's
/// merge queue. Returns whether every key was queued.
///
/// # Errors
///
/// A run-store or output failure.
pub fn unqueue(
    store: &RunStore,
    scope: &str,
    keys: &[String],
    out: &mut dyn Write,
) -> Result<bool, DispatchCliError> {
    let mut all = true;
    for key in keys {
        let key = key.to_uppercase();
        if store.dequeue_merge(scope, &key)? {
            writeln!(out, "removed {key} from the merge queue")?;
        } else {
            writeln!(out, "{key} was not queued")?;
            all = false;
        }
    }
    Ok(all)
}

/// Writes `scope`'s merge epic and merge queue, one indented row per
/// ticket in merge order with how it got there and its
/// [`crate::work::auto_merge::RowState`]. Writes nothing for a scope with
/// neither.
///
/// # Errors
///
/// A run-store or output failure.
pub fn write_queue(
    store: &RunStore,
    scope: &str,
    runs: &[OverviewRun],
    out: &mut dyn Write,
) -> Result<(), DispatchCliError> {
    if let Some(epic) = store.merge_epic(scope)? {
        writeln!(out, "  merge epic: {epic}")?;
    }
    let queue = store.merge_queue(scope)?;
    if queue.is_empty() {
        return Ok(());
    }
    writeln!(out, "  merge queue:")?;
    for (i, entry) in queue.iter().enumerate() {
        writeln!(
            out,
            "    {}. {} ({}) {}",
            i + 1,
            entry.ticket,
            entry.source.as_str(),
            row_state(entry, runs)
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::{TempDir, tempdir};

    const SCOPE: &str = "github:o/r";

    fn store() -> (TempDir, RunStore) {
        let dir = tempdir().unwrap();
        let store = RunStore::open(&dir.path().join("runs.db")).unwrap();
        (dir, store)
    }

    fn repo() -> CurrentRepo {
        CurrentRepo {
            scope: SCOPE.to_string(),
            repo_dir: PathBuf::from("/repos/r"),
        }
    }

    fn text(f: impl FnOnce(&mut Vec<u8>)) -> String {
        let mut out = Vec::new();
        f(&mut out);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn queue_appends_uppercased_keys_and_records_the_repo() {
        let (_dir, store) = store();
        let out = text(|out| {
            queue(
                &store,
                &repo(),
                &["gh-2".to_string(), "GH-1".to_string()],
                out,
            )
            .unwrap();
            queue(&store, &repo(), &["GH-2".to_string()], out).unwrap();
        });

        assert_eq!(
            out,
            "queued GH-2 to merge (1 of 1)\n\
             queued GH-1 to merge (2 of 2)\n\
             GH-2 is already queued (1 of 2)\n"
        );
        let rows = store.autopilot_scopes().unwrap();
        assert_eq!(rows[0].repo_dir.as_deref(), Some("/repos/r"));
        assert!(!rows[0].enabled, "queueing never turns autopilot on");
    }

    #[test]
    fn unqueue_reports_keys_that_were_not_queued() {
        let (_dir, store) = store();
        store
            .enqueue_merge(SCOPE, "GH-1", QueueSource::Manual)
            .unwrap();
        let mut all = true;
        let out = text(|out| {
            all = unqueue(
                &store,
                SCOPE,
                &["gh-1".to_string(), "GH-9".to_string()],
                out,
            )
            .unwrap();
        });

        assert!(!all);
        assert_eq!(
            out,
            "removed GH-1 from the merge queue\nGH-9 was not queued\n"
        );
        assert!(store.merge_queue(SCOPE).unwrap().is_empty());
    }

    #[test]
    fn write_queue_lists_rows_in_order_with_their_state() {
        let (_dir, store) = store();
        store.set_merge_epic(SCOPE, Some("GH-100")).unwrap();
        store
            .enqueue_merge(SCOPE, "GH-2", QueueSource::Manual)
            .unwrap();
        store
            .enqueue_merge(SCOPE, "GH-1", QueueSource::Autopilot)
            .unwrap();
        store
            .set_merge_waiting(SCOPE, "GH-2", Some("checks pending"))
            .unwrap();
        store.mark_merge_started(SCOPE, "GH-1", "abc").unwrap();
        store.hold_merge(SCOPE, "GH-1", "conflicts").unwrap();
        let runs = store.overview_runs().unwrap();

        let out = text(|out| write_queue(&store, SCOPE, &runs, out).unwrap());

        assert_eq!(
            out,
            "  merge epic: GH-100\n\
             \x20 merge queue:\n\
             \x20   1. GH-2 (manual) waiting: checks pending\n\
             \x20   2. GH-1 (autopilot) held: conflicts\n"
        );
    }

    #[test]
    fn write_queue_writes_nothing_for_an_empty_scope() {
        let (_dir, store) = store();
        let out = text(|out| write_queue(&store, SCOPE, &[], out).unwrap());
        assert!(out.is_empty());
    }
}
