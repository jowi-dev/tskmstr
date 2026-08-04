//! `tm runs`, `tm runs start`, and `tm runs finish`.
//!
//! Thin wrappers around [`crate::runs::RunStore`] that format its output for
//! the terminal. `start` and `finish` are meant to be invoked by a runner
//! (or its hooks) rather than typed interactively, so `start` prints only
//! the bare run id — easy for a shell to capture into a variable.

use std::io::Write;

use thiserror::Error;

use crate::runs::{FinishRun, RunStore, RunStoreError, RunSummary, StartRun};

/// Errors surfaced by `tm runs` subcommands.
#[derive(Debug, Error)]
pub enum RunsCliError {
    /// A [`RunStore`] operation failed.
    #[error(transparent)]
    Store(#[from] RunStoreError),

    /// Writing output failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// `tm runs start`: record the start of a lane run, printing only the new
/// run id (and a trailing newline) so shells can capture it directly.
pub fn start(store: &RunStore, params: &StartRun, out: &mut dyn Write) -> Result<(), RunsCliError> {
    let id = store.start_run(params)?;
    writeln!(out, "{id}")?;
    Ok(())
}

/// `tm runs finish`: record a run's terminal outcome.
///
/// Prints `Finished run {id}: {status}` with `status` lowercased, matching
/// the string [`crate::runs::RunStatus::as_str`] stores in the database.
pub fn finish(
    store: &RunStore,
    run_id: i64,
    outcome: &FinishRun,
    out: &mut dyn Write,
) -> Result<(), RunsCliError> {
    store.finish_run(run_id, outcome)?;
    writeln!(out, "Finished run {run_id}: {}", outcome.status.as_str())?;
    Ok(())
}

/// `tm runs` (no subcommand): list all recorded runs in an aligned table.
///
/// Prints `No runs recorded.` instead of an empty table when there are none.
pub fn list(store: &RunStore, out: &mut dyn Write) -> Result<(), RunsCliError> {
    let runs = store.list_runs()?;

    if runs.is_empty() {
        writeln!(out, "No runs recorded.")?;
        return Ok(());
    }

    let rows: Vec<[String; 5]> = runs
        .iter()
        .map(|run| {
            [
                run.ticket.clone(),
                run.lane.clone(),
                run.status.as_str().to_string(),
                format_age(run.age_secs),
                last_event_column(run),
            ]
        })
        .collect();

    let headers = ["TICKET", "LANE", "STATUS", "AGE", "LAST EVENT"];
    let mut widths = headers.map(str::len);
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }

    writeln!(out, "{}", format_row(&headers.map(str::to_string), &widths))?;
    for row in &rows {
        writeln!(out, "{}", format_row(row, &widths))?;
    }

    Ok(())
}

/// Format `row` as space-padded columns per `widths`, except the last
/// column, which is left unpadded (no trailing whitespace on each line).
fn format_row(row: &[String; 5], widths: &[usize; 5]) -> String {
    let mut parts = Vec::with_capacity(row.len());
    for (i, cell) in row.iter().enumerate() {
        if i + 1 == row.len() {
            parts.push(cell.clone());
        } else {
            parts.push(format!("{cell:<width$}", width = widths[i]));
        }
    }
    parts.join("  ")
}

/// Render a [`RunSummary`]'s last-event column: `{kind} {age} ago`, or `-`
/// when the run has no recorded events.
fn last_event_column(run: &RunSummary) -> String {
    match (&run.last_event_kind, run.last_event_age_secs) {
        (Some(kind), Some(age_secs)) => format!("{kind} {} ago", format_age(age_secs)),
        _ => "-".to_string(),
    }
}

/// Format `secs` as a short human-readable age.
///
/// Buckets: under a minute as `{s}s`; under an hour as `{m}m`; under a day
/// as `{h}h{mm:02}m`; otherwise `{d}d{h}h`. Negative values (clock skew)
/// clamp to `0s`.
pub fn format_age(secs: i64) -> String {
    let secs = secs.max(0);

    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        format!("{h}h{m:02}m")
    } else {
        let d = secs / 86400;
        let h = (secs % 86400) / 3600;
        format!("{d}d{h}h")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runs::RunStatus;
    use tempfile::tempdir;

    fn open_store(dir: &std::path::Path) -> RunStore {
        RunStore::open(&dir.join("runs.db")).expect("open should succeed")
    }

    fn start_params(ticket: &str) -> StartRun {
        StartRun {
            ticket: ticket.to_string(),
            lane: "backend".to_string(),
            worktree: "/tmp/wt".to_string(),
            branch: None,
            pid: None,
        }
    }

    #[test]
    fn start_prints_only_the_run_id() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let mut out = Vec::new();

        start(&store, &start_params("PROJ-1"), &mut out).expect("should succeed");

        assert_eq!(String::from_utf8(out).unwrap(), "1\n");
    }

    #[test]
    fn finish_prints_run_id_and_lowercase_status() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        let mut out = Vec::new();

        finish(
            &store,
            id,
            &FinishRun {
                status: RunStatus::Done,
                ..FinishRun::default()
            },
            &mut out,
        )
        .expect("should succeed");

        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("Finished run {id}: done\n")
        );
    }

    #[test]
    fn finish_unknown_run_id_errors_and_prints_nothing() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let mut out = Vec::new();

        let err = finish(&store, 999, &FinishRun::default(), &mut out).expect_err("should fail");

        assert!(matches!(
            err,
            RunsCliError::Store(RunStoreError::RunNotFound(999))
        ));
        assert!(out.is_empty());
    }

    #[test]
    fn list_with_no_runs_prints_no_runs_recorded() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let mut out = Vec::new();

        list(&store, &mut out).expect("should succeed");

        assert_eq!(String::from_utf8(out).unwrap(), "No runs recorded.\n");
    }

    #[test]
    fn list_prints_header_and_row_for_a_run_with_no_events() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        store.start_run(&start_params("PROJ-1")).unwrap();
        let mut out = Vec::new();

        list(&store, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        let mut lines = output.lines();
        assert_eq!(
            lines.next(),
            Some("TICKET  LANE     STATUS   AGE  LAST EVENT")
        );
        let row = lines.next().expect("should have a data row");
        assert_eq!(row, "PROJ-1  backend  running  0s   -");
        assert!(lines.next().is_none());
    }

    #[test]
    fn last_event_column_formats_kind_and_age_when_present() {
        let run = RunSummary {
            id: 1,
            ticket: "PROJ-1".to_string(),
            lane: "backend".to_string(),
            status: RunStatus::Running,
            age_secs: 120,
            heartbeat_age_secs: Some(5),
            last_event_kind: Some("tool_use".to_string()),
            last_event_age_secs: Some(45),
        };

        assert_eq!(last_event_column(&run), "tool_use 45s ago");
    }

    #[test]
    fn last_event_column_is_dash_when_absent() {
        let run = RunSummary {
            id: 1,
            ticket: "PROJ-1".to_string(),
            lane: "backend".to_string(),
            status: RunStatus::Running,
            age_secs: 120,
            heartbeat_age_secs: Some(5),
            last_event_kind: None,
            last_event_age_secs: None,
        };

        assert_eq!(last_event_column(&run), "-");
    }

    #[test]
    fn format_age_seconds_bucket() {
        assert_eq!(format_age(0), "0s");
        assert_eq!(format_age(45), "45s");
        assert_eq!(format_age(59), "59s");
    }

    #[test]
    fn format_age_minutes_bucket() {
        assert_eq!(format_age(60), "1m");
        assert_eq!(format_age(180), "3m");
        assert_eq!(format_age(3599), "59m");
    }

    #[test]
    fn format_age_hours_bucket() {
        assert_eq!(format_age(3600), "1h00m");
        assert_eq!(format_age(7505), "2h05m");
        assert_eq!(format_age(86399), "23h59m");
    }

    #[test]
    fn format_age_days_bucket() {
        assert_eq!(format_age(86400), "1d0h");
        assert_eq!(format_age(86400 + 4 * 3600), "1d4h");
    }

    #[test]
    fn format_age_negative_clamps_to_zero_seconds() {
        assert_eq!(format_age(-5), "0s");
    }
}
