//! `tm runs`, `tm runs start`, and `tm runs finish`.
//!
//! Thin wrappers around [`crate::runs::RunStore`] that format its output for
//! the terminal. `start` and `finish` are meant to be invoked by a runner
//! (or its hooks) rather than typed interactively, so `start` prints only
//! the bare run id — easy for a shell to capture into a variable.

use std::io::Write;

use thiserror::Error;

use crate::runs::{FinishRun, RunEvent, RunStore, RunStoreError, RunSummary, StartRun};

/// `tm runs reap`: mark abandoned runs (stale heartbeat, dead pid) as failed.
///
/// Prints `Reaped run {id} ({ticket})` for each reaped run, or
/// `Nothing to reap.` when none qualified.
pub fn reap(
    store: &RunStore,
    stale_after_mins: u64,
    pid_alive: &dyn Fn(u32) -> bool,
    out: &mut dyn Write,
) -> Result<(), RunsCliError> {
    let reaped = store.reap(stale_after_mins, pid_alive)?;

    if reaped.is_empty() {
        writeln!(out, "Nothing to reap.")?;
        return Ok(());
    }

    for run in &reaped {
        writeln!(out, "Reaped run {} ({})", run.id, run.ticket)?;
    }
    Ok(())
}

/// Errors surfaced by `tm runs` subcommands.
#[derive(Debug, Error)]
pub enum RunsCliError {
    /// A [`RunStore`] operation failed.
    #[error(transparent)]
    Store(#[from] RunStoreError),

    /// Writing output failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// `--detail` was given but isn't valid JSON.
    #[error("--detail is not valid JSON: {0}")]
    InvalidDetailJson(#[from] serde_json::Error),

    /// `--model-usage` was given but isn't a JSON object.
    #[error("--model-usage is not a valid JSON object: {0}")]
    InvalidModelUsageJson(String),

    /// `tm runs show`/`resume` was given a ticket with no recorded runs.
    #[error("no runs recorded for {ticket}")]
    NoRunForTicket {
        /// The ticket key that was looked up.
        ticket: String,
    },

    /// `tm runs resume` was given a ticket whose latest run has no session id.
    #[error(
        "latest run {run_id} for {ticket} has no session id; was it finished with --session-id?"
    )]
    NoSessionId {
        /// The ticket key that was looked up.
        ticket: String,
        /// The run id that has no session id.
        run_id: i64,
    },
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
    if let Some(model_usage) = &outcome.model_usage {
        let value: serde_json::Value = serde_json::from_str(model_usage)
            .map_err(|e| RunsCliError::InvalidModelUsageJson(e.to_string()))?;
        if !value.is_object() {
            return Err(RunsCliError::InvalidModelUsageJson(
                "expected a JSON object".to_string(),
            ));
        }
    }

    store.finish_run(run_id, outcome)?;
    writeln!(out, "Finished run {run_id}: {}", outcome.status.as_str())?;
    Ok(())
}

/// `tm runs event`: append a telemetry event to a run and bump its
/// heartbeat.
///
/// If `detail` is given, it is validated as JSON before the store is
/// touched at all, so an invalid `--detail` never results in a partially
/// recorded event. Prints `Recorded {kind} for run {run_id}` on success.
pub fn event(
    store: &RunStore,
    run_id: i64,
    kind: &str,
    detail: Option<&str>,
    out: &mut dyn Write,
) -> Result<(), RunsCliError> {
    if let Some(detail) = detail {
        serde_json::from_str::<serde_json::Value>(detail)?;
    }

    store.add_event(run_id, kind, detail)?;
    writeln!(out, "Recorded {kind} for run {run_id}")?;
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

/// `tm runs show`: print the latest run for `ticket` and its event timeline.
///
/// When `json` is `true`, prints a single pretty-printed JSON object instead
/// of the human-oriented rendering (see [`show_json`] for the schema) and
/// nothing else.
///
/// # Errors
///
/// Returns [`RunsCliError::NoRunForTicket`] if `ticket` has no recorded runs.
pub fn show(
    store: &RunStore,
    ticket: &str,
    json: bool,
    out: &mut dyn Write,
) -> Result<(), RunsCliError> {
    if json {
        return show_json(store, ticket, out);
    }

    let ticket = ticket.to_uppercase();
    let run =
        store
            .latest_run_for_ticket(&ticket)?
            .ok_or_else(|| RunsCliError::NoRunForTicket {
                ticket: ticket.clone(),
            })?;

    writeln!(
        out,
        "Run {}: {} [{}] {}",
        run.id,
        run.ticket,
        run.lane,
        run.status.as_str()
    )?;
    writeln!(out, "started {}", run.started_at)?;
    if let Some(ended_at) = &run.ended_at {
        writeln!(out, "ended {ended_at}")?;
    }
    if let Some(session_id) = &run.session_id {
        writeln!(out, "session {session_id}")?;
    }
    if let Some(pr_url) = &run.pr_url {
        writeln!(out, "pr {pr_url}")?;
    }
    if let Some(blocker) = &run.blocker {
        writeln!(out, "blocker {blocker}")?;
    }
    if run.cost_usd.is_some() || run.num_turns.is_some() {
        let cost = run
            .cost_usd
            .map(|c| format!("{c:.2}"))
            .unwrap_or_else(|| "?".to_string());
        let turns = run
            .num_turns
            .map(|t| t.to_string())
            .unwrap_or_else(|| "?".to_string());
        writeln!(out, "cost ${cost} / {turns} turns")?;
    }

    let events = store.events_for_run(run.id)?;

    if let Some(tools_line) = crate::runs::format_tool_counts(&crate::runs::tool_counts(&events)) {
        writeln!(out, "{tools_line}")?;
    }

    let authoritative_usage = run
        .model_usage
        .as_deref()
        .and_then(crate::runs::parse_model_usage);
    let (usage, usage_label) = match authoritative_usage {
        Some(usage) => (Some(usage), "Model usage"),
        None if run.status == crate::runs::RunStatus::Running => {
            (crate::runs::latest_usage(&events), "Model usage (live)")
        }
        None => (None, ""),
    };
    if let Some(usage) = usage {
        writeln!(out)?;
        writeln!(out, "{usage_label}")?;
        for line in crate::runs::format_model_usage(&usage) {
            writeln!(out, "{line}")?;
        }
    }

    let agent_usage_events = crate::runs::collect_agent_usage(&events);
    if !agent_usage_events.is_empty() {
        let agent_usage = crate::runs::aggregate_agent_usage(&agent_usage_events);
        writeln!(out)?;
        writeln!(out, "Agent usage")?;
        for line in crate::runs::format_agent_usage(&agent_usage) {
            writeln!(out, "{line}")?;
        }
    }

    if let Some(checklist) = crate::runs::latest_checklist(&events) {
        writeln!(out)?;
        writeln!(
            out,
            "Checklist ({}/{} done)",
            checklist.done_count(),
            checklist.items.len()
        )?;
        for item in &checklist.items {
            let marker = if item.done { "[x]" } else { "[ ]" };
            writeln!(out, "{marker} {}", item.text)?;
        }
    }

    writeln!(out)?;
    if events.is_empty() {
        writeln!(out, "(no events)")?;
    } else {
        for event in events.iter().rev() {
            writeln!(out, "{}", format_event_line(event))?;
        }
    }

    Ok(())
}

/// JSON projection of [`crate::runs::Run`] for [`show_json`]. Every field is
/// present (`null` rather than omitted) so downstream tooling can rely on a
/// stable schema regardless of which optionals happen to be set.
#[derive(serde::Serialize)]
struct RunJson<'a> {
    id: i64,
    ticket: &'a str,
    lane: &'a str,
    status: &'a str,
    session_id: Option<&'a str>,
    worktree: &'a str,
    branch: Option<&'a str>,
    pid: Option<u32>,
    transcript: Option<&'a str>,
    started_at: &'a str,
    heartbeat_at: Option<&'a str>,
    ended_at: Option<&'a str>,
    exit_code: Option<i32>,
    num_turns: Option<i64>,
    cost_usd: Option<f64>,
    blocker: Option<&'a str>,
    pr_url: Option<&'a str>,
    age_secs: i64,
}

/// JSON projection of one [`crate::runs::ChecklistItem`] for [`show_json`].
#[derive(serde::Serialize)]
struct ChecklistItemJson<'a> {
    text: &'a str,
    done: bool,
}

/// JSON projection of [`crate::runs::ChecklistState`] for [`show_json`].
#[derive(serde::Serialize)]
struct ChecklistJson<'a> {
    done: usize,
    total: usize,
    items: Vec<ChecklistItemJson<'a>>,
}

/// JSON projection of a run's model usage for [`show_json`]: the parsed
/// usage map alongside whether it came from the authoritative
/// `runs.model_usage` column (`"final"`) or a live `usage` event snapshot
/// (`"live"`) — the same distinction [`show`] labels "Model usage" vs.
/// "Model usage (live)".
#[derive(serde::Serialize)]
struct ModelUsageJson<'a> {
    source: &'static str,
    models: &'a crate::runs::ModelUsageMap,
}

/// JSON projection of one [`crate::runs::tool_counts`] entry for
/// [`show_json`].
#[derive(serde::Serialize)]
struct ToolCountJson<'a> {
    tool: &'a str,
    count: usize,
}

/// JSON projection of one `(agent_type, model)` -> [`crate::runs::AgentUsageTotals`]
/// aggregate for [`show_json`]. Field naming mirrors [`ModelUsageJson`]'s
/// token fields; unlike [`ModelUsageJson`] there is no `source`/`costUSD` —
/// per-agent cost is never available (see `docs/plans/per-agent-usage.md`).
/// Deliberately omits any derived "orchestrator" remainder row; consumers
/// can compute `model_usage total - sum(agent_usage)` themselves.
#[derive(serde::Serialize)]
struct AgentUsageJson<'a> {
    agent_type: &'a str,
    model: &'a str,
    invocations: u64,
    #[serde(rename = "outputTokens")]
    output_tokens: u64,
    #[serde(rename = "inputTokens")]
    input_tokens: u64,
    #[serde(rename = "cacheReadInputTokens")]
    cache_read_input_tokens: u64,
    #[serde(rename = "cacheCreationInputTokens")]
    cache_creation_input_tokens: u64,
    #[serde(rename = "totalToolUseCount")]
    total_tool_use_count: u64,
    #[serde(rename = "durationMs")]
    duration_ms: u64,
}

/// JSON projection of one [`RunEvent`] for [`show_json`]: `detail` is the
/// raw stored string verbatim, never the friendly rendering [`show`] uses.
#[derive(serde::Serialize)]
struct EventJson<'a> {
    at: &'a str,
    kind: &'a str,
    detail: Option<&'a str>,
}

/// Top-level JSON payload printed by `tm runs show --json`.
#[derive(serde::Serialize)]
struct ShowJson<'a> {
    run: RunJson<'a>,
    checklist: Option<ChecklistJson<'a>>,
    model_usage: Option<ModelUsageJson<'a>>,
    tool_counts: Vec<ToolCountJson<'a>>,
    agent_usage: Vec<AgentUsageJson<'a>>,
    events: Vec<EventJson<'a>>,
}

/// `tm runs show --json`: print the latest run for `ticket`, its checklist,
/// model usage, tool counts, and full event timeline as a single
/// pretty-printed JSON object, and nothing else.
///
/// Unlike [`show`]'s human-oriented rendering, `events` is
/// **oldest-first** (chronological order, matching
/// [`RunStore::events_for_run`]) and each event's `detail` is the raw stored
/// string verbatim — the newest-first ordering and friendly rendering
/// `show` uses are display-only concerns that don't belong in a stable,
/// machine-readable schema.
///
/// # Errors
///
/// Returns [`RunsCliError::NoRunForTicket`] if `ticket` has no recorded runs.
fn show_json(store: &RunStore, ticket: &str, out: &mut dyn Write) -> Result<(), RunsCliError> {
    let ticket = ticket.to_uppercase();
    let run =
        store
            .latest_run_for_ticket(&ticket)?
            .ok_or_else(|| RunsCliError::NoRunForTicket {
                ticket: ticket.clone(),
            })?;

    let events = store.events_for_run(run.id)?;

    let checklist_state = crate::runs::latest_checklist(&events);
    let checklist = checklist_state.as_ref().map(|checklist| ChecklistJson {
        done: checklist.done_count(),
        total: checklist.items.len(),
        items: checklist
            .items
            .iter()
            .map(|item| ChecklistItemJson {
                text: &item.text,
                done: item.done,
            })
            .collect(),
    });

    let authoritative_usage = run
        .model_usage
        .as_deref()
        .and_then(crate::runs::parse_model_usage);
    // Both branches must produce an owned `ModelUsageMap` so they unify:
    // the authoritative branch already owns one from `parse_model_usage`,
    // and the live branch owns one from `latest_usage`.
    let model_usage_data: Option<(crate::runs::ModelUsageMap, &'static str)> =
        match authoritative_usage {
            Some(models) => Some((models, "final")),
            None if run.status == crate::runs::RunStatus::Running => {
                crate::runs::latest_usage(&events).map(|models| (models, "live"))
            }
            None => None,
        };
    let model_usage = model_usage_data
        .as_ref()
        .map(|(models, source)| ModelUsageJson { source, models });

    let tool_counts_data = crate::runs::tool_counts(&events);
    let tool_counts: Vec<ToolCountJson> = tool_counts_data
        .iter()
        .map(|(tool, count)| ToolCountJson {
            tool,
            count: *count,
        })
        .collect();

    let agent_usage_events = crate::runs::collect_agent_usage(&events);
    let agent_usage_totals = crate::runs::aggregate_agent_usage(&agent_usage_events);
    let agent_usage: Vec<AgentUsageJson> = agent_usage_totals
        .iter()
        .map(|((agent_type, model), totals)| AgentUsageJson {
            agent_type,
            model,
            invocations: totals.invocations,
            output_tokens: totals.output_tokens,
            input_tokens: totals.input_tokens,
            cache_read_input_tokens: totals.cache_read_input_tokens,
            cache_creation_input_tokens: totals.cache_creation_input_tokens,
            total_tool_use_count: totals.total_tool_use_count,
            duration_ms: totals.duration_ms,
        })
        .collect();

    let events_json: Vec<EventJson> = events
        .iter()
        .map(|event| EventJson {
            at: &event.at,
            kind: &event.kind,
            detail: event.detail.as_deref(),
        })
        .collect();

    let payload = ShowJson {
        run: RunJson {
            id: run.id,
            ticket: &run.ticket,
            lane: &run.lane,
            status: run.status.as_str(),
            session_id: run.session_id.as_deref(),
            worktree: &run.worktree,
            branch: run.branch.as_deref(),
            pid: run.pid,
            transcript: run.transcript.as_deref(),
            started_at: &run.started_at,
            heartbeat_at: run.heartbeat_at.as_deref(),
            ended_at: run.ended_at.as_deref(),
            exit_code: run.exit_code,
            num_turns: run.num_turns,
            cost_usd: run.cost_usd,
            blocker: run.blocker.as_deref(),
            pr_url: run.pr_url.as_deref(),
            age_secs: run.age_secs,
        },
        checklist,
        model_usage,
        tool_counts,
        agent_usage,
        events: events_json,
    };

    let rendered = serde_json::to_string_pretty(&payload)
        .map_err(|e| RunsCliError::Io(std::io::Error::other(e.to_string())))?;
    writeln!(out, "{rendered}")?;
    Ok(())
}

/// Format one [`RunEvent`] as `{at}  {kind}  {detail}`, omitting the detail
/// segment when there is none. When [`crate::runs::format_event_detail`]
/// recognizes the event's kind and detail shape, the friendly rendering is
/// used in place of the raw detail JSON.
fn format_event_line(event: &RunEvent) -> String {
    match crate::runs::format_event_detail(&event.kind, event.detail.as_deref()) {
        Some(friendly) => format!("{}  {}  {}", event.at, event.kind, friendly),
        None => match &event.detail {
            Some(detail) => format!("{}  {}  {}", event.at, event.kind, detail),
            None => format!("{}  {}", event.at, event.kind),
        },
    }
}

/// `tm runs resume`: print the session id of the latest run of `ticket`, for
/// `claude --resume $(tm runs resume PROJ-1)`.
///
/// # Errors
///
/// Returns [`RunsCliError::NoRunForTicket`] if `ticket` has no recorded
/// runs, or [`RunsCliError::NoSessionId`] if its latest run has no session
/// id.
pub fn resume(store: &RunStore, ticket: &str, out: &mut dyn Write) -> Result<(), RunsCliError> {
    let ticket = ticket.to_uppercase();
    let run =
        store
            .latest_run_for_ticket(&ticket)?
            .ok_or_else(|| RunsCliError::NoRunForTicket {
                ticket: ticket.clone(),
            })?;

    let session_id = run.session_id.as_ref().ok_or(RunsCliError::NoSessionId {
        ticket: ticket.clone(),
        run_id: run.id,
    })?;

    writeln!(out, "{session_id}")?;
    Ok(())
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
    fn finish_stores_valid_model_usage_json() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        let mut out = Vec::new();

        finish(
            &store,
            id,
            &FinishRun {
                status: RunStatus::Done,
                model_usage: Some(r#"{"claude-fable-5":{"inputTokens":146}}"#.to_string()),
                ..FinishRun::default()
            },
            &mut out,
        )
        .expect("should succeed");

        let run = store.run_by_id(id).unwrap().expect("expected a run");
        assert_eq!(
            run.model_usage,
            Some(r#"{"claude-fable-5":{"inputTokens":146}}"#.to_string())
        );
    }

    #[test]
    fn finish_rejects_malformed_model_usage_json_and_stores_nothing() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        let mut out = Vec::new();

        let err = finish(
            &store,
            id,
            &FinishRun {
                status: RunStatus::Done,
                model_usage: Some("not json".to_string()),
                ..FinishRun::default()
            },
            &mut out,
        )
        .expect_err("should fail");

        assert!(matches!(err, RunsCliError::InvalidModelUsageJson(_)));
        assert!(out.is_empty());

        let run = store.run_by_id(id).unwrap().expect("expected a run");
        assert_eq!(run.model_usage, None);
        assert_eq!(run.status, RunStatus::Running);
    }

    #[test]
    fn finish_rejects_model_usage_json_that_is_not_an_object() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        let mut out = Vec::new();

        let err = finish(
            &store,
            id,
            &FinishRun {
                status: RunStatus::Done,
                model_usage: Some("[1,2,3]".to_string()),
                ..FinishRun::default()
            },
            &mut out,
        )
        .expect_err("should fail");

        assert!(matches!(err, RunsCliError::InvalidModelUsageJson(_)));
    }

    #[test]
    fn event_prints_confirmation_and_records_kind() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        let mut out = Vec::new();

        event(&store, id, "tool_use", Some(r#"{"file":"a.rs"}"#), &mut out)
            .expect("should succeed");

        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("Recorded tool_use for run {id}\n")
        );

        let mut list_out = Vec::new();
        list(&store, &mut list_out).unwrap();
        let list_output = String::from_utf8(list_out).unwrap();
        assert!(
            list_output.contains("tool_use"),
            "list output should show the recorded event kind: {list_output}"
        );
    }

    #[test]
    fn event_without_detail_succeeds() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        let mut out = Vec::new();

        event(&store, id, "stop", None, &mut out).expect("should succeed");

        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("Recorded stop for run {id}\n")
        );
    }

    #[test]
    fn event_invalid_json_detail_errors_and_inserts_nothing() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        let mut out = Vec::new();

        let err =
            event(&store, id, "tool_use", Some("not json"), &mut out).expect_err("should fail");

        assert!(matches!(err, RunsCliError::InvalidDetailJson(_)));
        assert!(out.is_empty());

        let mut list_out = Vec::new();
        list(&store, &mut list_out).unwrap();
        let list_output = String::from_utf8(list_out).unwrap();
        assert!(
            !list_output.contains("tool_use"),
            "no event should have been recorded: {list_output}"
        );
    }

    #[test]
    fn event_agent_usage_kind_rejects_non_json_detail() {
        // Pins that `--kind agent_usage` goes through the same generic
        // JSON validation as every other kind (see `event`'s doc comment)
        // rather than some kind-specific check, so a future refactor that
        // special-cases kinds can't silently drop this validation for
        // agent_usage.
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        let mut out = Vec::new();

        let err = event(&store, id, "agent_usage", Some("not json"), &mut out)
            .expect_err("should fail");

        assert!(matches!(err, RunsCliError::InvalidDetailJson(_)));
        assert!(out.is_empty());

        let mut list_out = Vec::new();
        list(&store, &mut list_out).unwrap();
        let list_output = String::from_utf8(list_out).unwrap();
        assert!(
            !list_output.contains("agent_usage"),
            "no event should have been recorded: {list_output}"
        );
    }

    #[test]
    fn event_unknown_run_id_errors() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let mut out = Vec::new();

        let err = event(&store, 999, "tool_use", None, &mut out).expect_err("should fail");

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

    fn always_alive(_pid: u32) -> bool {
        true
    }

    fn always_dead(_pid: u32) -> bool {
        false
    }

    #[test]
    fn reap_prints_nothing_to_reap_when_none_qualify() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        store.start_run(&start_params("PROJ-1")).unwrap();
        let mut out = Vec::new();

        reap(&store, 10, &always_alive, &mut out).expect("should succeed");

        assert_eq!(String::from_utf8(out).unwrap(), "Nothing to reap.\n");
    }

    #[test]
    fn reap_prints_a_line_per_reaped_run() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        // `RunStore` keeps its connection private, so this test can't
        // backdate the heartbeat directly (see runs::tests for that kind of
        // coverage). Instead, let a moment of real time pass and reap with
        // `--stale-after 0`, so the freshly-started run's `started_at` is
        // already "stale" relative to "now".
        std::thread::sleep(std::time::Duration::from_millis(10));
        let mut out = Vec::new();

        reap(&store, 0, &always_dead, &mut out).expect("should succeed");

        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("Reaped run {id} (PROJ-1)\n")
        );
    }

    #[test]
    fn show_prints_header_and_events_for_latest_run() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        store
            .finish_run(
                id,
                &FinishRun {
                    status: RunStatus::Done,
                    session_id: Some("sess-abc".to_string()),
                    cost_usd: Some(1.5),
                    num_turns: Some(3),
                    pr_url: Some("https://example.invalid/pr/1".to_string()),
                    ..FinishRun::default()
                },
            )
            .unwrap();
        event(
            &store,
            id,
            "tool_use",
            Some(r#"{"file":"a.rs"}"#),
            &mut Vec::new(),
        )
        .unwrap();
        event(&store, id, "stop", None, &mut Vec::new()).unwrap();

        let mut out = Vec::new();
        show(&store, "proj-1", false, &mut out).expect("should succeed");
        let output = String::from_utf8(out).unwrap();

        assert!(output.starts_with(&format!("Run {id}: PROJ-1 [backend] done\n")));
        assert!(output.contains("session sess-abc\n"));
        assert!(output.contains("pr https://example.invalid/pr/1\n"));
        assert!(output.contains("cost $1.50 / 3 turns\n"));
        assert!(output.contains("tool_use  {\"file\":\"a.rs\"}"));
        assert!(output.contains("  stop\n") || output.ends_with("  stop\n"));
        assert!(!output.contains("blocker"));
    }

    #[test]
    fn show_renders_friendly_tool_event_detail() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        event(
            &store,
            id,
            "tool",
            Some(r#"{"tool":"Bash","summary":"cargo test"}"#),
            &mut Vec::new(),
        )
        .unwrap();

        let mut out = Vec::new();
        show(&store, "PROJ-1", false, &mut out).expect("should succeed");
        let output = String::from_utf8(out).unwrap();

        assert!(output.contains("tool  Bash — cargo test"));
        assert!(!output.contains("\"tool\":\"Bash\""));
    }

    #[test]
    fn show_falls_back_to_raw_detail_for_unrecognized_shape() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        event(
            &store,
            id,
            "tool_use",
            Some(r#"{"file":"a.rs"}"#),
            &mut Vec::new(),
        )
        .unwrap();

        let mut out = Vec::new();
        show(&store, "PROJ-1", false, &mut out).expect("should succeed");
        let output = String::from_utf8(out).unwrap();

        assert!(output.contains("tool_use  {\"file\":\"a.rs\"}"));
    }

    #[test]
    fn show_prints_tools_summary_line_before_checklist() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        event(
            &store,
            id,
            "tool",
            Some(r#"{"tool":"Bash"}"#),
            &mut Vec::new(),
        )
        .unwrap();
        event(
            &store,
            id,
            "tool",
            Some(r#"{"tool":"Bash"}"#),
            &mut Vec::new(),
        )
        .unwrap();
        event(
            &store,
            id,
            "tool",
            Some(r#"{"tool":"Edit"}"#),
            &mut Vec::new(),
        )
        .unwrap();
        event(
            &store,
            id,
            "checklist",
            Some(r#"{"items":[{"text":"write tests","done":true}]}"#),
            &mut Vec::new(),
        )
        .unwrap();

        let mut out = Vec::new();
        show(&store, "PROJ-1", false, &mut out).expect("should succeed");
        let output = String::from_utf8(out).unwrap();

        assert!(output.contains("Tools: Bash \u{d7}2, Edit \u{d7}1"));
        let tools_pos = output.find("Tools:").unwrap();
        let checklist_pos = output.find("Checklist").unwrap();
        assert!(
            tools_pos < checklist_pos,
            "expected Tools summary before checklist section: {output}"
        );
    }

    #[test]
    fn show_with_no_tool_events_has_no_tools_line() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        event(&store, id, "stop", None, &mut Vec::new()).unwrap();

        let mut out = Vec::new();
        show(&store, "PROJ-1", false, &mut out).expect("should succeed");
        let output = String::from_utf8(out).unwrap();

        assert!(!output.contains("Tools:"));
    }

    #[test]
    fn show_prints_events_newest_first() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        event(&store, id, "first", None, &mut Vec::new()).unwrap();
        event(&store, id, "second", None, &mut Vec::new()).unwrap();

        let mut out = Vec::new();
        show(&store, "PROJ-1", false, &mut out).expect("should succeed");
        let output = String::from_utf8(out).unwrap();

        let first_pos = output.find("second").expect("second event present");
        let second_pos = output.find("first").expect("first event present");
        assert!(
            first_pos < second_pos,
            "expected newest event (second) to print before oldest event (first): {output}"
        );
    }

    #[test]
    fn show_prints_checklist_section_above_events() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        event(
            &store,
            id,
            "checklist",
            Some(r#"{"items":[{"text":"write tests","done":true},{"text":"implement","done":false}]}"#),
            &mut Vec::new(),
        )
        .unwrap();

        let mut out = Vec::new();
        show(&store, "PROJ-1", false, &mut out).expect("should succeed");
        let output = String::from_utf8(out).unwrap();

        assert!(output.contains("Checklist (1/2 done)"));
        assert!(output.contains("[x] write tests"));
        assert!(output.contains("[ ] implement"));
        let checklist_pos = output.find("Checklist").unwrap();
        let event_pos = output.find("checklist  1/2 done").unwrap();
        assert!(
            checklist_pos < event_pos,
            "expected checklist section before event timeline: {output}"
        );
    }

    #[test]
    fn show_with_no_checklist_has_no_checklist_section() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        event(&store, id, "tool_use", None, &mut Vec::new()).unwrap();

        let mut out = Vec::new();
        show(&store, "PROJ-1", false, &mut out).expect("should succeed");
        let output = String::from_utf8(out).unwrap();

        assert!(!output.contains("Checklist"));
    }

    #[test]
    fn show_prefers_authoritative_model_usage_over_live_events() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        event(
            &store,
            id,
            "usage",
            Some(r#"{"models":{"claude-fable-5":{"outputTokens":1}}}"#),
            &mut Vec::new(),
        )
        .unwrap();
        finish(
            &store,
            id,
            &FinishRun {
                status: RunStatus::Done,
                model_usage: Some(
                    r#"{"claude-fable-5":{"outputTokens":58564,"costUSD":12.996}}"#.to_string(),
                ),
                ..FinishRun::default()
            },
            &mut Vec::new(),
        )
        .unwrap();

        let mut out = Vec::new();
        show(&store, "PROJ-1", false, &mut out).expect("should succeed");
        let output = String::from_utf8(out).unwrap();

        assert!(output.contains("Model usage"));
        assert!(!output.contains("Model usage (live)"));
        assert!(output.contains("$13.00"));
        assert!(output.contains("out 58.6k"));
    }

    #[test]
    fn show_falls_back_to_live_usage_events_while_running() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        event(
            &store,
            id,
            "usage",
            Some(r#"{"models":{"claude-fable-5":{"outputTokens":58564}}}"#),
            &mut Vec::new(),
        )
        .unwrap();

        let mut out = Vec::new();
        show(&store, "PROJ-1", false, &mut out).expect("should succeed");
        let output = String::from_utf8(out).unwrap();

        assert!(output.contains("Model usage (live)"));
        assert!(output.contains("out 58.6k"));
        assert!(!output.contains('$'));
    }

    #[test]
    fn show_with_no_model_usage_has_no_model_usage_section() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        finish(
            &store,
            id,
            &FinishRun {
                status: RunStatus::Done,
                ..FinishRun::default()
            },
            &mut Vec::new(),
        )
        .unwrap();

        let mut out = Vec::new();
        show(&store, "PROJ-1", false, &mut out).expect("should succeed");
        let output = String::from_utf8(out).unwrap();

        assert!(!output.contains("Model usage"));
    }

    const AGENT_USAGE_FIXTURE: &str = r#"{"agentType": "elixir-implementer", "description": "Implement AX-404 UI threading", "model": "claude-sonnet-5", "outputTokens": 1143, "inputTokens": 2, "cacheReadInputTokens": 87519, "cacheCreationInputTokens": 3012, "totalToolUseCount": 38, "durationMs": 193659}"#;

    #[test]
    fn show_prints_agent_usage_section_after_model_usage() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        finish(
            &store,
            id,
            &FinishRun {
                status: RunStatus::Done,
                model_usage: Some(
                    r#"{"claude-sonnet-5":{"outputTokens":58564,"costUSD":12.996}}"#.to_string(),
                ),
                ..FinishRun::default()
            },
            &mut Vec::new(),
        )
        .unwrap();
        event(
            &store,
            id,
            "agent_usage",
            Some(AGENT_USAGE_FIXTURE),
            &mut Vec::new(),
        )
        .unwrap();

        let mut out = Vec::new();
        show(&store, "PROJ-1", false, &mut out).expect("should succeed");
        let output = String::from_utf8(out).unwrap();

        assert!(output.contains("Agent usage"));
        assert!(output.contains("elixir-implementer"));
        assert!(output.contains("out 1.1k"));

        let model_usage_pos = output.find("Model usage").unwrap();
        let agent_usage_pos = output.find("Agent usage").unwrap();
        assert!(
            model_usage_pos < agent_usage_pos,
            "expected Agent usage section after Model usage: {output}"
        );
    }

    #[test]
    fn show_with_no_agent_usage_events_has_no_agent_usage_section() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        event(&store, id, "tool_use", None, &mut Vec::new()).unwrap();

        let mut out = Vec::new();
        show(&store, "PROJ-1", false, &mut out).expect("should succeed");
        let output = String::from_utf8(out).unwrap();

        assert!(!output.contains("Agent usage"));
    }

    #[test]
    fn show_with_no_events_prints_placeholder() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        store.start_run(&start_params("PROJ-1")).unwrap();

        let mut out = Vec::new();
        show(&store, "PROJ-1", false, &mut out).expect("should succeed");
        let output = String::from_utf8(out).unwrap();

        assert!(output.contains("(no events)"));
        assert!(!output.contains("ended "));
        assert!(!output.contains("session "));
    }

    #[test]
    fn show_unknown_ticket_errors() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let mut out = Vec::new();

        let err = show(&store, "PROJ-404", false, &mut out).expect_err("should fail");

        assert!(matches!(
            err,
            RunsCliError::NoRunForTicket { ticket } if ticket == "PROJ-404"
        ));
        assert!(out.is_empty());
    }

    #[test]
    fn show_json_unknown_ticket_errors_and_prints_nothing() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let mut out = Vec::new();

        let err = show(&store, "PROJ-404", true, &mut out).expect_err("should fail");

        assert!(matches!(
            err,
            RunsCliError::NoRunForTicket { ticket } if ticket == "PROJ-404"
        ));
        assert!(out.is_empty());
    }

    #[test]
    fn show_json_happy_path_has_expected_fields() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        store
            .finish_run(
                id,
                &FinishRun {
                    status: RunStatus::Done,
                    session_id: Some("sess-abc".to_string()),
                    cost_usd: Some(1.5),
                    num_turns: Some(3),
                    pr_url: Some("https://example.invalid/pr/1".to_string()),
                    ..FinishRun::default()
                },
            )
            .unwrap();
        event(
            &store,
            id,
            "tool",
            Some(r#"{"tool":"Bash"}"#),
            &mut Vec::new(),
        )
        .unwrap();
        event(&store, id, "stop", None, &mut Vec::new()).unwrap();

        let mut out = Vec::new();
        show(&store, "proj-1", true, &mut out).expect("should succeed");
        let output = String::from_utf8(out).unwrap();

        let value: serde_json::Value =
            serde_json::from_str(&output).expect("output should be valid JSON");

        assert_eq!(value["run"]["id"], id);
        assert_eq!(value["run"]["ticket"], "PROJ-1");
        assert_eq!(value["run"]["lane"], "backend");
        assert_eq!(value["run"]["status"], "done");
        assert_eq!(value["run"]["session_id"], "sess-abc");
        assert_eq!(value["run"]["worktree"], "/tmp/wt");
        assert_eq!(value["run"]["branch"], serde_json::Value::Null);
        assert_eq!(value["run"]["pid"], serde_json::Value::Null);
        assert_eq!(value["run"]["cost_usd"], 1.5);
        assert_eq!(value["run"]["num_turns"], 3);
        assert_eq!(value["run"]["pr_url"], "https://example.invalid/pr/1");
        assert_eq!(value["run"]["blocker"], serde_json::Value::Null);
        assert!(value["run"]["started_at"].is_string());
        assert!(value["run"]["age_secs"].is_number());

        assert_eq!(
            value["tool_counts"],
            serde_json::json!([{"tool": "Bash", "count": 1}])
        );

        let events = value["events"].as_array().expect("events should be array");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["kind"], "tool");
        assert_eq!(events[0]["detail"], r#"{"tool":"Bash"}"#);
        assert_eq!(events[1]["kind"], "stop");
        assert_eq!(events[1]["detail"], serde_json::Value::Null);
    }

    #[test]
    fn show_json_checklist_and_model_usage_are_null_when_absent() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        store.start_run(&start_params("PROJ-1")).unwrap();

        let mut out = Vec::new();
        show(&store, "PROJ-1", true, &mut out).expect("should succeed");
        let value: serde_json::Value = serde_json::from_str(&String::from_utf8(out).unwrap())
            .expect("output should be valid JSON");

        assert_eq!(value["checklist"], serde_json::Value::Null);
        assert_eq!(value["model_usage"], serde_json::Value::Null);
        assert_eq!(value["tool_counts"], serde_json::json!([]));
        assert_eq!(value["events"], serde_json::json!([]));
    }

    #[test]
    fn show_json_includes_checklist_done_total_and_items() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        event(
            &store,
            id,
            "checklist",
            Some(r#"{"items":[{"text":"write tests","done":true},{"text":"implement","done":false}]}"#),
            &mut Vec::new(),
        )
        .unwrap();

        let mut out = Vec::new();
        show(&store, "PROJ-1", true, &mut out).expect("should succeed");
        let value: serde_json::Value = serde_json::from_str(&String::from_utf8(out).unwrap())
            .expect("output should be valid JSON");

        assert_eq!(value["checklist"]["done"], 1);
        assert_eq!(value["checklist"]["total"], 2);
        assert_eq!(
            value["checklist"]["items"],
            serde_json::json!([
                {"text": "write tests", "done": true},
                {"text": "implement", "done": false},
            ])
        );
    }

    #[test]
    fn show_json_model_usage_source_is_final_when_authoritative() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        event(
            &store,
            id,
            "usage",
            Some(r#"{"models":{"claude-fable-5":{"outputTokens":1}}}"#),
            &mut Vec::new(),
        )
        .unwrap();
        finish(
            &store,
            id,
            &FinishRun {
                status: RunStatus::Done,
                model_usage: Some(
                    r#"{"claude-fable-5":{"outputTokens":58564,"costUSD":12.996}}"#.to_string(),
                ),
                ..FinishRun::default()
            },
            &mut Vec::new(),
        )
        .unwrap();

        let mut out = Vec::new();
        show(&store, "PROJ-1", true, &mut out).expect("should succeed");
        let value: serde_json::Value = serde_json::from_str(&String::from_utf8(out).unwrap())
            .expect("output should be valid JSON");

        assert_eq!(value["model_usage"]["source"], "final");
        assert_eq!(
            value["model_usage"]["models"]["claude-fable-5"]["outputTokens"],
            58564
        );
        assert_eq!(
            value["model_usage"]["models"]["claude-fable-5"]["costUSD"],
            12.996
        );
    }

    #[test]
    fn show_json_model_usage_source_is_live_while_running() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        event(
            &store,
            id,
            "usage",
            Some(r#"{"models":{"claude-fable-5":{"outputTokens":58564}}}"#),
            &mut Vec::new(),
        )
        .unwrap();

        let mut out = Vec::new();
        show(&store, "PROJ-1", true, &mut out).expect("should succeed");
        let value: serde_json::Value = serde_json::from_str(&String::from_utf8(out).unwrap())
            .expect("output should be valid JSON");

        assert_eq!(value["model_usage"]["source"], "live");
        assert_eq!(
            value["model_usage"]["models"]["claude-fable-5"]["outputTokens"],
            58564
        );
        assert!(
            value["model_usage"]["models"]["claude-fable-5"]
                .get("costUSD")
                .is_none()
        );
    }

    #[test]
    fn show_json_agent_usage_is_empty_array_when_absent() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        store.start_run(&start_params("PROJ-1")).unwrap();

        let mut out = Vec::new();
        show(&store, "PROJ-1", true, &mut out).expect("should succeed");
        let value: serde_json::Value = serde_json::from_str(&String::from_utf8(out).unwrap())
            .expect("output should be valid JSON");

        assert_eq!(value["agent_usage"], serde_json::json!([]));
    }

    #[test]
    fn show_json_agent_usage_has_expected_shape() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        event(
            &store,
            id,
            "agent_usage",
            Some(AGENT_USAGE_FIXTURE),
            &mut Vec::new(),
        )
        .unwrap();

        let mut out = Vec::new();
        show(&store, "PROJ-1", true, &mut out).expect("should succeed");
        let value: serde_json::Value = serde_json::from_str(&String::from_utf8(out).unwrap())
            .expect("output should be valid JSON");

        let agent_usage = value["agent_usage"]
            .as_array()
            .expect("agent_usage should be an array");
        assert_eq!(agent_usage.len(), 1);
        let row = &agent_usage[0];
        assert_eq!(row["agent_type"], "elixir-implementer");
        assert_eq!(row["model"], "claude-sonnet-5");
        assert_eq!(row["invocations"], 1);
        assert_eq!(row["outputTokens"], 1143);
        assert_eq!(row["inputTokens"], 2);
        assert_eq!(row["cacheReadInputTokens"], 87519);
        assert_eq!(row["cacheCreationInputTokens"], 3012);
        assert_eq!(row["totalToolUseCount"], 38);
        assert_eq!(row["durationMs"], 193659);
        // No derived orchestrator remainder row per the plan.
        assert!(
            agent_usage
                .iter()
                .all(|row| row["agent_type"] != "orchestrator")
        );
    }

    #[test]
    fn show_json_agent_usage_matches_human_aggregation_for_repeated_agent_type() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        event(
            &store,
            id,
            "agent_usage",
            Some(AGENT_USAGE_FIXTURE),
            &mut Vec::new(),
        )
        .unwrap();
        event(
            &store,
            id,
            "agent_usage",
            Some(AGENT_USAGE_FIXTURE),
            &mut Vec::new(),
        )
        .unwrap();

        let mut out = Vec::new();
        show(&store, "PROJ-1", true, &mut out).expect("should succeed");
        let value: serde_json::Value = serde_json::from_str(&String::from_utf8(out).unwrap())
            .expect("output should be valid JSON");

        let agent_usage = value["agent_usage"].as_array().unwrap();
        assert_eq!(
            agent_usage.len(),
            1,
            "two invocations of the same (agent_type, model) should aggregate to one row"
        );
        assert_eq!(agent_usage[0]["invocations"], 2);
        assert_eq!(agent_usage[0]["outputTokens"], 1143 * 2);

        let mut human_out = Vec::new();
        show(&store, "PROJ-1", false, &mut human_out).expect("should succeed");
        let human_output = String::from_utf8(human_out).unwrap();
        assert!(human_output.contains("2x"));
        assert!(human_output.contains("out 2.3k"));
    }

    #[test]
    fn show_json_events_are_oldest_first_with_raw_detail() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        event(
            &store,
            id,
            "tool",
            Some(r#"{"tool":"Bash","summary":"cargo test"}"#),
            &mut Vec::new(),
        )
        .unwrap();
        event(&store, id, "second", None, &mut Vec::new()).unwrap();

        let mut out = Vec::new();
        show(&store, "PROJ-1", true, &mut out).expect("should succeed");
        let value: serde_json::Value = serde_json::from_str(&String::from_utf8(out).unwrap())
            .expect("output should be valid JSON");

        let events = value["events"].as_array().unwrap();
        assert_eq!(events[0]["kind"], "tool");
        assert_eq!(
            events[0]["detail"],
            r#"{"tool":"Bash","summary":"cargo test"}"#
        );
        assert_eq!(events[1]["kind"], "second");
    }

    #[test]
    fn resume_prints_only_the_session_id() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        store
            .finish_run(
                id,
                &FinishRun {
                    status: RunStatus::Done,
                    session_id: Some("sess-abc".to_string()),
                    ..FinishRun::default()
                },
            )
            .unwrap();

        let mut out = Vec::new();
        resume(&store, "proj-1", &mut out).expect("should succeed");

        assert_eq!(String::from_utf8(out).unwrap(), "sess-abc\n");
    }

    #[test]
    fn resume_unknown_ticket_errors() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let mut out = Vec::new();

        let err = resume(&store, "PROJ-404", &mut out).expect_err("should fail");

        assert!(matches!(
            err,
            RunsCliError::NoRunForTicket { ticket } if ticket == "PROJ-404"
        ));
        assert!(out.is_empty());
    }

    #[test]
    fn resume_no_session_id_errors_with_run_id_in_message() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        store
            .finish_run(
                id,
                &FinishRun {
                    status: RunStatus::Done,
                    ..FinishRun::default()
                },
            )
            .unwrap();

        let mut out = Vec::new();
        let err = resume(&store, "PROJ-1", &mut out).expect_err("should fail");

        match &err {
            RunsCliError::NoSessionId { ticket, run_id } => {
                assert_eq!(ticket, "PROJ-1");
                assert_eq!(*run_id, id);
            }
            other => panic!("expected NoSessionId, got {other:?}"),
        }
        assert_eq!(
            err.to_string(),
            format!(
                "latest run {id} for PROJ-1 has no session id; was it finished with --session-id?"
            )
        );
        assert!(out.is_empty());
    }
}
