//! `tm usage`: inspect the `usage_snapshots` the GH-114 usage gate reads,
//! and `tm usage statusline`, a Claude Code statusline shim that keeps them
//! fresh while you work interactively (GitHub issue #130).
//!
//! Claude Code hands its statusline command a JSON payload on stdin on every
//! render. When the session is on a subscription that payload carries
//! `rate_limits` with the current 5-hour and 7-day utilization. The shim
//! saves those windows (source `statusline`) and then runs your real
//! statusline command with the same stdin bytes, so it wraps an existing
//! statusline rather than replacing it. See
//! `docs/plans/gh-113-proactive-usage-signal.md`.

use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use crate::cli::runs::format_age;
use crate::runs::{RunStore, RunStoreError, UsageSnapshotRow};

/// The agent statusline snapshots are recorded under: the only one that
/// renders a statusline with usage windows.
pub use crate::agent::STATUSLINE_AGENT;

/// The `source` column value for snapshots written by the shim.
pub const STATUSLINE_SOURCE: &str = "statusline";

/// The windows the shim records. A gateway also reports `spend_limit`,
/// which the usage gate doesn't use.
const STATUSLINE_WINDOWS: [&str; 2] = ["five_hour", "seven_day"];

/// Minimum seconds between two statusline writes. The statusline renders
/// many times a minute, while the gate only needs a snapshot that is
/// minutes fresh.
pub const WRITE_INTERVAL_SECS: i64 = 30;

/// Errors from recording a statusline payload. [`statusline`] swallows
/// every one of them.
#[derive(Debug, thiserror::Error)]
pub enum UsageError {
    /// The stdin payload wasn't JSON.
    #[error("statusline payload is not JSON: {0}")]
    Parse(#[from] serde_json::Error),
    /// The run store couldn't be opened or written.
    #[error(transparent)]
    Store(#[from] RunStoreError),
    /// The throttle stamp couldn't be written.
    #[error("could not write the statusline stamp: {0}")]
    Stamp(#[from] io::Error),
}

/// What [`record_statusline`] did with one payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordOutcome {
    /// The payload had no usable `rate_limits` (not yet known, or an API
    /// key / Bedrock / Vertex session). Nothing was written.
    NoRateLimits,
    /// The last write was under [`WRITE_INTERVAL_SECS`] ago, so the run
    /// store wasn't opened.
    Throttled,
    /// The windows were upserted and the stamp was updated.
    Written,
}

/// Extracts the `five_hour` and `seven_day` windows from a statusline
/// payload as `usage_snapshots` rows observed at `observed_at`.
///
/// `used_percentage` is divided by 100 to match the stream's
/// `utilization` fraction. A missing `rate_limits` key, or a window
/// without both `used_percentage` and `resets_at`, yields no row for it.
pub fn parse_statusline(
    input: &[u8],
    observed_at: i64,
) -> Result<Vec<UsageSnapshotRow>, serde_json::Error> {
    let payload: serde_json::Value = serde_json::from_slice(input)?;
    let Some(rate_limits) = payload.get("rate_limits") else {
        return Ok(Vec::new());
    };
    Ok(STATUSLINE_WINDOWS
        .iter()
        .filter_map(|&window| {
            let entry = rate_limits.get(window)?;
            let used = entry.get("used_percentage")?.as_f64()?;
            let resets_at = entry.get("resets_at")?.as_f64()? as i64;
            Some(UsageSnapshotRow {
                agent: STATUSLINE_AGENT.to_string(),
                window: window.to_string(),
                utilization: used / 100.0,
                resets_at,
                observed_at,
                source: STATUSLINE_SOURCE.to_string(),
                status: None,
            })
        })
        .collect())
}

/// Records the payload's windows into the run store opened by
/// `open_store`, newest-wins, unless the last write (the unix seconds kept
/// in `stamp_path`) was under [`WRITE_INTERVAL_SECS`] before `now`. A
/// payload with no windows never touches the stamp or the store.
pub fn record_statusline(
    input: &[u8],
    now: i64,
    stamp_path: &Path,
    open_store: impl FnOnce() -> Result<RunStore, RunStoreError>,
) -> Result<RecordOutcome, UsageError> {
    let rows = parse_statusline(input, now)?;
    if rows.is_empty() {
        return Ok(RecordOutcome::NoRateLimits);
    }

    let last_write = std::fs::read_to_string(stamp_path)
        .ok()
        .and_then(|text| text.trim().parse::<i64>().ok());
    if last_write.is_some_and(|last| now - last < WRITE_INTERVAL_SECS) {
        return Ok(RecordOutcome::Throttled);
    }

    let store = open_store()?;
    for row in &rows {
        store.upsert_usage_snapshot(row)?;
    }
    std::fs::write(stamp_path, now.to_string())?;
    Ok(RecordOutcome::Written)
}

/// Runs `argv` with `input` on its stdin, copies its stdout to `out`
/// unchanged, and returns its exit code. Its stderr is inherited. An empty
/// `argv` prints nothing and returns `0`. A command that can't be started
/// returns `127`, as a shell would.
pub fn passthrough(argv: &[String], input: &[u8], out: &mut dyn Write) -> i32 {
    let Some((program, args)) = argv.split_first() else {
        return 0;
    };
    let mut child = match Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            eprintln!("tm usage statusline: {program}: {err}");
            return 127;
        }
    };

    // Feed stdin from a thread so a command that writes before it has read
    // everything can't deadlock against us. A command that exits without
    // reading closes the pipe; that write error is its business.
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let input = input.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });

    let mut output = Vec::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_end(&mut output);
    }
    let _ = writer.join();
    let _ = out.write_all(&output);
    let _ = out.flush();

    match child.wait() {
        Ok(status) => status.code().unwrap_or(1),
        Err(_) => 1,
    }
}

/// `tm usage statusline [-- <command...>]`: records `input` (see
/// [`record_statusline`]), then hands it to `argv` (see [`passthrough`])
/// and returns the passthrough's exit code.
///
/// Recording never fails the statusline: any error is written to `debug`
/// and the passthrough still runs, with the same output and exit code.
#[allow(clippy::too_many_arguments)]
pub fn statusline(
    argv: &[String],
    input: &[u8],
    now: i64,
    stamp_path: &Path,
    open_store: impl FnOnce() -> Result<RunStore, RunStoreError>,
    out: &mut dyn Write,
    debug: &mut dyn Write,
) -> i32 {
    if let Err(err) = record_statusline(input, now, stamp_path, open_store) {
        let _ = writeln!(debug, "tm usage statusline: {err}");
    }
    passthrough(argv, input, out)
}

/// `tm usage`: prints every stored snapshot with its utilization, source,
/// age and reset time relative to `now`.
pub fn list(rows: &[UsageSnapshotRow], now: i64, out: &mut dyn Write) -> io::Result<()> {
    if rows.is_empty() {
        return writeln!(out, "no usage snapshots recorded");
    }

    writeln!(
        out,
        "{:<8} {:<10} {:>5}  {:<10} {:<15} {:<10} RESETS",
        "AGENT", "WINDOW", "USED", "SOURCE", "STATUS", "OBSERVED"
    )?;
    for row in rows {
        let resets = if row.resets_at > now {
            format!("in {}", format_age(row.resets_at - now))
        } else {
            format!("reset {} ago", format_age(now - row.resets_at))
        };
        writeln!(
            out,
            "{:<8} {:<10} {:>5}  {:<10} {:<15} {:<10} {resets}",
            row.agent,
            row.window,
            format!("{:.0}%", row.utilization * 100.0),
            row.source,
            row.status.as_deref().unwrap_or("-"),
            format!("{} ago", format_age(now - row.observed_at)),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A statusline payload trimmed from a live interactive session (see
    /// the GH-113 spike), plus a gateway's `spend_limit` window.
    const PAYLOAD: &str = r#"{
        "session_id": "abc",
        "model": {"id": "claude-opus-5-5", "display_name": "Opus"},
        "workspace": {"current_dir": "/tmp"},
        "rate_limits": {
            "five_hour": {"used_percentage": 34, "resets_at": 1792365600},
            "seven_day": {"used_percentage": 7.5, "resets_at": 1792735200},
            "spend_limit": {"used_percentage": 50, "resets_at": 1792735200}
        }
    }"#;

    const NO_RATE_LIMITS: &str = r#"{"session_id": "abc", "model": {"id": "x"}}"#;

    fn row(window: &str, utilization: f64, resets_at: i64, observed_at: i64) -> UsageSnapshotRow {
        UsageSnapshotRow {
            agent: STATUSLINE_AGENT.to_string(),
            window: window.to_string(),
            utilization,
            resets_at,
            observed_at,
            source: STATUSLINE_SOURCE.to_string(),
            status: None,
        }
    }

    fn open(dir: &Path) -> impl FnOnce() -> Result<RunStore, RunStoreError> {
        let path = dir.join("runs.db");
        move || RunStore::open(&path)
    }

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn parse_statusline_reads_five_hour_and_seven_day_as_fractions() {
        let rows = parse_statusline(PAYLOAD.as_bytes(), 1000).unwrap();
        assert_eq!(
            rows,
            vec![
                row("five_hour", 0.34, 1_792_365_600, 1000),
                row("seven_day", 0.075, 1_792_735_200, 1000),
            ]
        );
    }

    #[test]
    fn parse_statusline_without_rate_limits_is_empty() {
        assert!(
            parse_statusline(NO_RATE_LIMITS.as_bytes(), 1000)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn parse_statusline_skips_a_window_missing_a_field() {
        let input = r#"{"rate_limits": {
            "five_hour": {"used_percentage": 34},
            "seven_day": {"used_percentage": 7, "resets_at": 1792735200}
        }}"#;
        assert_eq!(
            parse_statusline(input.as_bytes(), 1000).unwrap(),
            vec![row("seven_day", 0.07, 1_792_735_200, 1000)]
        );
    }

    #[test]
    fn parse_statusline_rejects_non_json() {
        assert!(parse_statusline(b"not json", 1000).is_err());
    }

    #[test]
    fn record_statusline_upserts_the_windows() {
        let dir = tempfile::tempdir().unwrap();
        let stamp = dir.path().join("stamp");

        let outcome =
            record_statusline(PAYLOAD.as_bytes(), 1000, &stamp, open(dir.path())).unwrap();

        assert_eq!(outcome, RecordOutcome::Written);
        let store = RunStore::open(&dir.path().join("runs.db")).unwrap();
        assert_eq!(
            store.usage_snapshots(STATUSLINE_AGENT).unwrap(),
            vec![
                row("five_hour", 0.34, 1_792_365_600, 1000),
                row("seven_day", 0.075, 1_792_735_200, 1000),
            ]
        );
    }

    #[test]
    fn record_statusline_without_rate_limits_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let stamp = dir.path().join("stamp");

        let outcome = record_statusline(NO_RATE_LIMITS.as_bytes(), 1000, &stamp, || {
            panic!("the store must not be opened")
        })
        .unwrap();

        assert_eq!(outcome, RecordOutcome::NoRateLimits);
        assert!(!stamp.exists());
    }

    #[test]
    fn record_statusline_throttles_writes_by_the_injected_clock() {
        let dir = tempfile::tempdir().unwrap();
        let stamp = dir.path().join("stamp");
        let later = PAYLOAD.replace("34", "40");

        assert_eq!(
            record_statusline(PAYLOAD.as_bytes(), 1000, &stamp, open(dir.path())).unwrap(),
            RecordOutcome::Written
        );
        assert_eq!(
            record_statusline(
                later.as_bytes(),
                1000 + WRITE_INTERVAL_SECS - 1,
                &stamp,
                || { panic!("a throttled write must not open the store") }
            )
            .unwrap(),
            RecordOutcome::Throttled
        );
        assert_eq!(
            record_statusline(
                later.as_bytes(),
                1000 + WRITE_INTERVAL_SECS,
                &stamp,
                open(dir.path())
            )
            .unwrap(),
            RecordOutcome::Written
        );

        let store = RunStore::open(&dir.path().join("runs.db")).unwrap();
        let five_hour = &store.usage_snapshots(STATUSLINE_AGENT).unwrap()[0];
        assert_eq!(five_hour.utilization, 0.40);
        assert_eq!(five_hour.observed_at, 1000 + WRITE_INTERVAL_SECS);
    }

    #[test]
    fn record_statusline_treats_an_unreadable_stamp_as_no_prior_write() {
        let dir = tempfile::tempdir().unwrap();
        let stamp = dir.path().join("stamp");
        std::fs::write(&stamp, "garbage").unwrap();

        assert_eq!(
            record_statusline(PAYLOAD.as_bytes(), 1000, &stamp, open(dir.path())).unwrap(),
            RecordOutcome::Written
        );
    }

    #[test]
    fn passthrough_hands_the_command_identical_stdin_and_copies_its_stdout() {
        let mut out = Vec::new();
        let code = passthrough(&argv(&["cat"]), PAYLOAD.as_bytes(), &mut out);
        assert_eq!(code, 0);
        assert_eq!(out, PAYLOAD.as_bytes());
    }

    #[test]
    fn passthrough_returns_the_commands_exit_code() {
        let mut out = Vec::new();
        let code = passthrough(&argv(&["sh", "-c", "printf hi; exit 3"]), b"", &mut out);
        assert_eq!(code, 3);
        assert_eq!(out, b"hi");
    }

    #[test]
    fn passthrough_with_no_command_prints_nothing() {
        let mut out = Vec::new();
        assert_eq!(passthrough(&[], PAYLOAD.as_bytes(), &mut out), 0);
        assert!(out.is_empty());
    }

    #[test]
    fn passthrough_of_a_missing_command_returns_127() {
        let mut out = Vec::new();
        let code = passthrough(&argv(&["/nonexistent/tm-statusline"]), b"", &mut out);
        assert_eq!(code, 127);
        assert!(out.is_empty());
    }

    #[test]
    fn statusline_records_and_passes_through() {
        let dir = tempfile::tempdir().unwrap();
        let stamp = dir.path().join("stamp");
        let (mut out, mut debug) = (Vec::new(), Vec::new());

        let code = statusline(
            &argv(&["cat"]),
            PAYLOAD.as_bytes(),
            1000,
            &stamp,
            open(dir.path()),
            &mut out,
            &mut debug,
        );

        assert_eq!(code, 0);
        assert_eq!(out, PAYLOAD.as_bytes());
        assert!(debug.is_empty());
        let store = RunStore::open(&dir.path().join("runs.db")).unwrap();
        assert_eq!(store.usage_snapshots(STATUSLINE_AGENT).unwrap().len(), 2);
    }

    #[test]
    fn statusline_errors_never_change_the_output_or_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let stamp = dir.path().join("stamp");
        let command = argv(&["sh", "-c", "cat; exit 2"]);

        for input in [&b"not json"[..], PAYLOAD.as_bytes()] {
            let (mut out, mut debug) = (Vec::new(), Vec::new());
            let code = statusline(
                &command,
                input,
                1000,
                &stamp,
                || {
                    Err(RunStoreError::CreateDir {
                        path: "/nope".into(),
                        source: io::Error::other("denied"),
                    })
                },
                &mut out,
                &mut debug,
            );

            assert_eq!(code, 2);
            assert_eq!(out, input);
            assert!(!debug.is_empty(), "the error is debug-logged");
        }
    }

    #[test]
    fn list_prints_each_snapshot_with_age_and_reset() {
        let rows = vec![
            row("five_hour", 0.34, 1000 + 3 * 3600 + 12 * 60, 1000 - 120),
            UsageSnapshotRow {
                source: "stream".to_string(),
                status: Some("allowed".to_string()),
                ..row("seven_day", 1.05, 1000 + 2 * 86400, 1000 - 7200)
            },
        ];
        let mut out = Vec::new();

        list(&rows, 1000, &mut out).unwrap();

        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert!(lines[0].starts_with("AGENT"), "{text}");
        for needle in [
            "claude",
            "five_hour",
            "34%",
            "statusline",
            "2m ago",
            "in 3h12m",
        ] {
            assert!(lines[1].contains(needle), "{needle} missing: {}", lines[1]);
        }
        for needle in [
            "seven_day",
            "105%",
            "stream",
            "allowed",
            "2h00m ago",
            "in 2d0h",
        ] {
            assert!(lines[2].contains(needle), "{needle} missing: {}", lines[2]);
        }
    }

    #[test]
    fn list_marks_a_window_that_has_already_reset() {
        let mut out = Vec::new();
        list(&[row("five_hour", 0.9, 900, 800)], 1000, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("reset 1m ago"), "{text}");
    }

    #[test]
    fn list_with_no_snapshots_says_so() {
        let mut out = Vec::new();
        list(&[], 1000, &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "no usage snapshots recorded\n"
        );
    }
}
