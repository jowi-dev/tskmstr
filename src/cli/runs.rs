//! `tm runs`, `tm runs start`, and `tm runs finish`.
//!
//! Thin wrappers around [`crate::runs::RunStore`] that format its output for
//! the terminal. `start` and `finish` are meant to be invoked by a runner
//! (or its hooks) rather than typed interactively, so `start` prints only
//! the bare run id — easy for a shell to capture into a variable.

use std::io::Write;
use std::path::Path;

use thiserror::Error;

use crate::runs::session::{SessionEnv, register_session};
use crate::runs::{
    FinishRun, Run, RunEvent, RunStatus, RunStore, RunStoreError, RunSummary, StartRun,
};

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

    /// `tm runs reopen` was given a numeric id with no matching run row.
    #[error("no run with id {0}")]
    NoRunWithId(i64),

    /// `tm runs logs` resolved a run with no recorded `log_path` and no
    /// by-convention fallback for its `kind` (see [`fallback_log_path`]).
    #[error(
        "run {id} ({ticket}) has no recorded log path, and its kind has no \
         by-convention fallback; see `tm runs show {ticket}` for its event timeline"
    )]
    NoLogPath {
        /// The ticket key the run belongs to.
        ticket: String,
        /// The run's row id.
        id: i64,
    },

    /// `tm runs logs` resolved a log path, but nothing exists there.
    #[error("log file {path} does not exist")]
    LogFileMissing {
        /// The path that was resolved but not found.
        path: std::path::PathBuf,
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
    let mut outcome = outcome.clone();

    if let Some(model_usage) = &outcome.model_usage {
        let value: serde_json::Value = serde_json::from_str(model_usage)
            .map_err(|e| RunsCliError::InvalidModelUsageJson(e.to_string()))?;
        if !value.is_object() {
            return Err(RunsCliError::InvalidModelUsageJson(
                "expected a JSON object".to_string(),
            ));
        }

        // Interactive audit/create sessions have no authoritative cost the
        // way a lane run's claude -p result does (see
        // crate::runs::pricing's module docs) — a bare `--model-usage` map
        // with no costUSD (as tm-session-end.sh reports) gets an ESTIMATED
        // one filled in here, and rolled up into `cost_usd` when the caller
        // didn't already provide one explicitly. A lane run's map already
        // carries costUSD for every model, so estimate_missing_costs is a
        // no-op there.
        if let Some(mut map) = crate::runs::parse_model_usage(model_usage) {
            crate::runs::estimate_missing_costs(&mut map);
            outcome.model_usage = Some(
                serde_json::to_string(&map).expect("a parsed ModelUsageMap always re-serializes"),
            );
            if outcome.cost_usd.is_none() {
                outcome.cost_usd = crate::runs::total_cost_usd(&map);
            }
        }
    }

    store.finish_run(run_id, &outcome)?;
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

/// `tm runs` (no subcommand): list recorded runs in an aligned table,
/// restricted to `kind` when given (see [`RunStore::list_runs_filtered`]).
///
/// Prints `No runs recorded.` instead of an empty table when there are none.
pub fn list(store: &RunStore, kind: Option<&str>, out: &mut dyn Write) -> Result<(), RunsCliError> {
    let runs = store.list_runs_filtered(kind)?;

    if runs.is_empty() {
        writeln!(out, "No runs recorded.")?;
        return Ok(());
    }

    let rows: Vec<[String; 6]> = runs
        .iter()
        .map(|run| {
            [
                run.ticket.clone(),
                run.lane.clone(),
                run.kind.clone(),
                run.status.as_str().to_string(),
                format_age(run.age_secs),
                last_event_column(run),
            ]
        })
        .collect();

    let headers = ["TICKET", "LANE", "KIND", "STATUS", "AGE", "LAST EVENT"];
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

/// `tm runs --by-outcome`: print a cost-vs-outcome summary joining
/// [`crate::runs::RunStore::cost_by_findings_outcome`]'s three buckets
/// (not measured / clean / findings), restricted to `kind` when given.
///
/// Always prints all three rows, even ones with zero runs, so "not
/// measured" stays visible rather than silently vanishing from the
/// comparison -- collapsing it into "clean" is exactly the miscount this
/// view exists to avoid. A bucket with no `cost_usd` data (no runs, or
/// runs whose cost was never recorded) renders `-` for its cost columns
/// rather than `$0.00`, which would misrepresent "unknown" as "free".
pub fn list_by_outcome(
    store: &RunStore,
    kind: Option<&str>,
    out: &mut dyn Write,
) -> Result<(), RunsCliError> {
    let summary = store.cost_by_findings_outcome(kind)?;

    let rows: Vec<[String; 4]> = summary
        .iter()
        .map(|s| {
            [
                s.outcome.as_str().to_string(),
                s.run_count.to_string(),
                format_optional_cost(s.total_cost_usd),
                format_optional_cost(s.avg_cost_usd),
            ]
        })
        .collect();

    let headers = ["OUTCOME", "RUNS", "TOTAL COST", "AVG COST"];
    let mut widths = headers.map(str::len);
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }

    writeln!(
        out,
        "{}",
        format_outcome_row(&headers.map(str::to_string), &widths)
    )?;
    for row in &rows {
        writeln!(out, "{}", format_outcome_row(row, &widths))?;
    }

    Ok(())
}

/// `tm runs --by-retro`: print a cost-vs-retro-verdict summary joining
/// [`crate::runs::RunStore::cost_by_retro_verdict`]'s two buckets (clean /
/// defect), restricted to `kind` when given.
///
/// Always prints both rows, even ones with zero tickets, same
/// stable-shape/visibility rationale as [`list_by_outcome`]. A `TICKETS W/O
/// RUN` column reports, per bucket, how many of its tickets have no
/// recorded run at all -- those tickets are excluded from the cost columns
/// entirely rather than folded in as a `$0` run, since a shipped ticket with
/// no run usually means the work was done manually rather than by a lane.
pub fn list_by_retro(
    store: &RunStore,
    kind: Option<&str>,
    out: &mut dyn Write,
) -> Result<(), RunsCliError> {
    let summary = store.cost_by_retro_verdict(kind)?;

    let rows: Vec<[String; 6]> = summary
        .iter()
        .map(|s| {
            [
                s.verdict.as_str().to_string(),
                s.ticket_count.to_string(),
                s.tickets_without_run.to_string(),
                s.run_count.to_string(),
                format_optional_cost(s.total_cost_usd),
                format_optional_cost(s.avg_cost_usd),
            ]
        })
        .collect();

    let headers = [
        "VERDICT",
        "TICKETS",
        "TICKETS W/O RUN",
        "RUNS",
        "TOTAL COST",
        "AVG COST",
    ];
    let mut widths = headers.map(str::len);
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }

    writeln!(
        out,
        "{}",
        format_retro_row(&headers.map(str::to_string), &widths)
    )?;
    for row in &rows {
        writeln!(out, "{}", format_retro_row(row, &widths))?;
    }

    Ok(())
}

/// Same left-padding-except-last-column rule as [`format_row`], for
/// [`list_by_retro`]'s 6-column table.
fn format_retro_row(row: &[String; 6], widths: &[usize; 6]) -> String {
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

/// Renders an optional cost as `$N.NN`, or `-` when there's no cost data to
/// report -- distinct from `$0.00`, which would claim a known zero cost.
fn format_optional_cost(cost_usd: Option<f64>) -> String {
    cost_usd
        .map(|c| format!("${c:.2}"))
        .unwrap_or_else(|| "-".to_string())
}

/// Same left-padding-except-last-column rule as [`format_row`], for
/// [`list_by_outcome`]'s 4-column table.
fn format_outcome_row(row: &[String; 4], widths: &[usize; 4]) -> String {
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

/// Format `row` as space-padded columns per `widths`, except the last
/// column, which is left unpadded (no trailing whitespace on each line).
fn format_row(row: &[String; 6], widths: &[usize; 6]) -> String {
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

/// Renders a run's `findings_count` for [`show`]'s human-oriented output,
/// keeping "not measured" (`None`), "measured, clean" (`Some(0)`), and a
/// nonzero tally visually distinct so a NULL never reads as a clean run.
fn format_findings_count(findings_count: Option<i64>) -> String {
    match findings_count {
        None => "not measured".to_string(),
        Some(0) => "0 (clean)".to_string(),
        Some(n) => n.to_string(),
    }
}

/// Render a [`RunSummary`]'s last-event column: `{kind} {age} ago`, or `-`
/// when the run has no recorded events.
fn last_event_column(run: &RunSummary) -> String {
    match (&run.last_event_kind, run.last_event_age_secs) {
        (Some(kind), Some(age_secs)) => format!("{kind} {} ago", format_age(age_secs)),
        _ => "-".to_string(),
    }
}

/// Resolves `ticket_or_id` to a [`Run`] for `tm runs reopen`: a value that
/// parses as an `i64` is looked up by row id ([`RunStore::run_by_id`]),
/// disambiguating multiple runs sharing a ticket the same way `tm runs
/// watch`'s detail window does; anything else is uppercased and resolved as
/// a ticket key via [`RunStore::latest_run_for_ticket_kind`] -- the same
/// ticket/`--kind` resolution `tm runs show` uses -- restricted to `kind`
/// when given (`kind` is ignored for a numeric id, since a row id is already
/// unambiguous).
///
/// # Errors
///
/// Returns [`RunsCliError::NoRunWithId`] for an unmatched numeric id, or
/// [`RunsCliError::NoRunForTicket`] for a ticket with no recorded runs (of
/// `kind`, if given).
pub(crate) fn resolve_run(
    store: &RunStore,
    ticket_or_id: &str,
    kind: Option<&str>,
) -> Result<Run, RunsCliError> {
    if let Ok(id) = ticket_or_id.parse::<i64>() {
        return store.run_by_id(id)?.ok_or(RunsCliError::NoRunWithId(id));
    }

    let ticket = ticket_or_id.to_uppercase();
    store
        .latest_run_for_ticket_kind(&ticket, kind)?
        .ok_or(RunsCliError::NoRunForTicket { ticket })
}

/// `tm runs show`: print the latest run for `ticket` and its event timeline.
///
/// When `kind` is given, restricts to the latest run of that kind (see
/// [`RunStore::latest_run_for_ticket_kind`]) instead of the latest run of
/// any kind — disambiguates a session run shadowing a lane run for the same
/// ticket. When `json` is `true`, prints a single pretty-printed JSON object
/// instead of the human-oriented rendering (see [`show_json`] for the
/// schema) and nothing else.
///
/// # Errors
///
/// Returns [`RunsCliError::NoRunForTicket`] if `ticket` has no recorded runs
/// (of `kind`, if given).
pub fn show(
    store: &RunStore,
    ticket: &str,
    kind: Option<&str>,
    json: bool,
    out: &mut dyn Write,
) -> Result<(), RunsCliError> {
    if json {
        return show_json(store, ticket, kind, out);
    }

    let ticket = ticket.to_uppercase();
    let run = store
        .latest_run_for_ticket_kind(&ticket, kind)?
        .ok_or_else(|| RunsCliError::NoRunForTicket {
            ticket: ticket.clone(),
        })?;

    writeln!(
        out,
        "Run {}: {} [{}/{}] {}",
        run.id,
        run.ticket,
        run.lane,
        run.kind,
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
    let authoritative_usage = run
        .model_usage
        .as_deref()
        .and_then(crate::runs::parse_model_usage);
    let cost_is_estimated = authoritative_usage
        .as_ref()
        .is_some_and(crate::runs::model_usage_is_estimated);

    if run.cost_usd.is_some() || run.num_turns.is_some() {
        let cost = run
            .cost_usd
            .map(|c| {
                if cost_is_estimated {
                    format!("~${c:.2}")
                } else {
                    format!("${c:.2}")
                }
            })
            .unwrap_or_else(|| "?".to_string());
        let turns = run
            .num_turns
            .map(|t| t.to_string())
            .unwrap_or_else(|| "?".to_string());
        writeln!(out, "cost {cost} / {turns} turns")?;
    }
    writeln!(
        out,
        "findings {}",
        format_findings_count(run.findings_count)
    )?;

    let events = store.events_for_run(run.id)?;

    if let Some(tools_line) = crate::runs::format_tool_counts(&crate::runs::tool_counts(&events)) {
        writeln!(out, "{tools_line}")?;
    }

    let (usage, usage_label) = match authoritative_usage {
        Some(usage) if cost_is_estimated => (Some(usage), "Model usage (estimated)"),
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
    kind: &'a str,
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
    /// Number of unresolved bot review findings tallied for this run; see
    /// [`crate::runs::FinishRun::findings_count`] for why `null` and `0`
    /// carry distinct meanings and neither is ever collapsed into the
    /// other here.
    findings_count: Option<i64>,
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

/// `tm runs show --json`: print the latest run for `ticket` (restricted to
/// `kind` when given, same as [`show`]), its checklist, model usage, tool
/// counts, and full event timeline as a single pretty-printed JSON object,
/// and nothing else.
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
/// Returns [`RunsCliError::NoRunForTicket`] if `ticket` has no recorded runs
/// (of `kind`, if given).
fn show_json(
    store: &RunStore,
    ticket: &str,
    kind: Option<&str>,
    out: &mut dyn Write,
) -> Result<(), RunsCliError> {
    let ticket = ticket.to_uppercase();
    let run = store
        .latest_run_for_ticket_kind(&ticket, kind)?
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
            // A "final" (authoritative) column can still hold an ESTIMATED
            // cost -- see crate::runs::pricing's module docs -- so this
            // checks per-model `estimated` rather than assuming the column
            // always means fully authoritative. Never conflate the two.
            Some(models) if crate::runs::model_usage_is_estimated(&models) => {
                Some((models, "estimated"))
            }
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
            kind: &run.kind,
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
            findings_count: run.findings_count,
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
pub fn resume(
    store: &RunStore,
    ticket: &str,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), RunsCliError> {
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

    // Doesn't block resuming (that would be a regression -- resuming a
    // finished session to keep chatting is normal usage), just points at
    // `tm runs reopen` for anyone who actually wants the run row itself
    // treated as live again (e.g. after the run.rs classification bug that
    // motivated RunStatus::Interrupted marked an in-flight run terminal).
    if run.status.is_terminal() {
        writeln!(
            err,
            "warning: run {} for {ticket} is already {} — `claude --resume` will work, \
             but the run row will still look finished. Run `tm runs reopen {ticket}` first \
             if you want it to look active again.",
            run.id,
            run.status.as_str()
        )?;
    }

    writeln!(out, "{session_id}")?;
    Ok(())
}

/// `tm runs reopen <ticket-or-run-id> [--kind <kind>] [--to <status>]`:
/// reopen a finished run so it can be worked (or resumed) again.
///
/// Resolves `ticket_or_id`/`kind` via [`resolve_run`] (same rules as `tm
/// runs show`, plus numeric ids), then reopens the resolved row via
/// [`RunStore::reopen_run`]. Prints `Reopened run {id} ({ticket}): {old} ->
/// {new}` on success.
///
/// # Errors
///
/// Returns [`RunsCliError::NoRunWithId`]/[`RunsCliError::NoRunForTicket`] if
/// nothing resolves, or the wrapped [`RunStoreError::NotTerminal`] if the
/// resolved run's status isn't terminal.
pub fn reopen(
    store: &RunStore,
    ticket_or_id: &str,
    kind: Option<&str>,
    to: RunStatus,
    out: &mut dyn Write,
) -> Result<(), RunsCliError> {
    let run = resolve_run(store, ticket_or_id, kind)?;
    let reopened = store.reopen_run(run.id, to)?;

    writeln!(
        out,
        "Reopened run {} ({}): {} -> {}",
        reopened.id,
        reopened.ticket,
        reopened.old_status.as_str(),
        reopened.new_status.as_str()
    )?;
    Ok(())
}

/// Default number of trailing lines [`logs`] prints when `--tail` isn't
/// given and `--follow` isn't set: generous enough to see a whole failed
/// `tm pr watch` run's worth of poll ticks (the default `poll_secs`/
/// `max_wait_mins` combination produces well under this many lines) without
/// dumping an unbounded file to the terminal.
pub const DEFAULT_LOG_TAIL_LINES: usize = 200;

/// The by-convention log path for a run with no recorded `log_path` column
/// (every run started before that column existed, including any `kind =
/// "review-watch"` run from before this fix — exactly what let the owner's
/// failed bugbot crons go undiagnosed).
///
/// Only `review-watch` has a convention derivable from `kind` + `ticket`
/// alone ([`crate::cli::pr::watch_log_dir`]): a lane run's log filename also
/// bears a worktree name and timestamp neither of which survive into the
/// `runs` row anywhere [`logs`] can recover them, so this returns `None` for
/// every other kind rather than guessing.
fn fallback_log_path(home: &Path, kind: &str, ticket: &str) -> Option<std::path::PathBuf> {
    if kind == "review-watch" {
        Some(crate::cli::pr::watch_log_dir(home).join(format!("{}.log", ticket.to_lowercase())))
    } else {
        None
    }
}

/// Resolves the log file path for `run`: its own `log_path` column if set,
/// otherwise [`fallback_log_path`]. `None` means there is truly no way to
/// find this run's log.
pub(crate) fn resolve_log_path(run: &Run, home: &Path) -> Option<std::path::PathBuf> {
    run.log_path
        .as_deref()
        .map(std::path::PathBuf::from)
        .or_else(|| fallback_log_path(home, &run.kind, &run.ticket))
}

/// Returns the last `n` lines of `content`, in original order. `n == 0`
/// yields an empty slice, and `content` shorter than `n` lines returns all
/// of it.
fn tail_lines(content: &str, n: usize) -> Vec<&str> {
    // `lines()` drops a trailing empty element from a final `\n`, matching
    // `wc -l`/`tail`'s idea of "how many lines" a file has.
    let all: Vec<&str> = content.lines().collect();
    let start = all.len().saturating_sub(n);
    all[start..].to_vec()
}

/// Reads whatever bytes have been appended to `path` since `offset`,
/// returning them alongside the file's new length (the next call's
/// `offset`). Used by [`logs`]'s `--follow` loop; pulled out as its own
/// function so the "what's new since last time" logic is testable without
/// an actual infinite polling loop.
fn read_appended(path: &Path, offset: u64) -> std::io::Result<(String, u64)> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    if len <= offset {
        return Ok((String::new(), offset));
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    Ok((buf, len))
}

/// `tm runs logs <ticket-or-run-id> [--kind <kind>] [--tail <N>] [--follow]`:
/// print (or follow) a run's detached-process log file.
///
/// Resolves `ticket_or_id`/`kind` via [`resolve_run`] (same rules as `tm
/// runs reopen`), then the log path via [`resolve_log_path`] — the recorded
/// `log_path` column, or [`fallback_log_path`] for the many pre-migration
/// rows (including the failed bugbot runs this whole feature exists for)
/// that predate it.
///
/// Three distinct "nothing to show" outcomes, each worded differently so the
/// caller knows which one they hit:
/// - [`RunsCliError::NoLogPath`]: no column value and no fallway for this
///   run's `kind`.
/// - [`RunsCliError::LogFileMissing`]: a path was resolved but nothing
///   exists there on disk.
/// - An empty (zero-byte) file is not an error: prints an explanatory line
///   pointing at `tm runs show` (see this module's doc comment) and returns
///   `Ok`, unless `--follow` is set, in which case it keeps watching for
///   content to appear.
///
/// `--follow`'s live loop is real (`sleeper.sleep`/re-reading the file
/// forever) and, like [`crate::work::detach::RealDetachSpawner`]'s actual
/// process spawn, is not itself unit-tested — only the pure pieces it's
/// built from ([`read_appended`], [`tail_lines`]) are. It never returns on
/// its own; the caller (a real terminal) is expected to Ctrl-C out, same as
/// `tail -f`.
#[allow(clippy::too_many_arguments)]
pub fn logs(
    store: &RunStore,
    home: &Path,
    ticket_or_id: &str,
    kind: Option<&str>,
    tail: usize,
    follow: bool,
    sleeper: &dyn crate::work::review_watch::Sleeper,
    out: &mut dyn Write,
) -> Result<(), RunsCliError> {
    let run = resolve_run(store, ticket_or_id, kind)?;
    let path = resolve_log_path(&run, home).ok_or_else(|| RunsCliError::NoLogPath {
        ticket: run.ticket.clone(),
        id: run.id,
    })?;

    if !path.exists() {
        return Err(RunsCliError::LogFileMissing { path });
    }

    let content = std::fs::read_to_string(&path)?;
    let mut offset = content.len() as u64;

    if content.is_empty() {
        writeln!(
            out,
            "log file {} is empty (no runtime output was captured for run {}); \
             see `tm runs show {}` for the recorded event timeline.",
            path.display(),
            run.id,
            run.ticket
        )?;
        if !follow {
            return Ok(());
        }
    } else {
        for line in tail_lines(&content, tail) {
            writeln!(out, "{line}")?;
        }
    }

    if follow {
        loop {
            sleeper.sleep(2);
            let (appended, new_offset) = read_appended(&path, offset)?;
            if !appended.is_empty() {
                write!(out, "{appended}")?;
                offset = new_offset;
            }
        }
    }

    Ok(())
}

/// `tm runs register --kind <KIND> <KEY>`: a thin wrapper around
/// [`register_session`], letting a skill invoked directly (e.g.
/// `/bugbot-triage`, which has no reason to call `tm ticket audit`/`create`)
/// adopt the same session-registration path those Rust commands already
/// call as their own first turn. See `docs/plans/bugbot-watch.md`'s
/// "Adoption" section.
///
/// No new logic beyond uppercasing `key` into a ticket: a no-op (does
/// nothing, prints nothing) when `env.session_id` is absent, matching
/// [`register_session`]'s own no-op contract. Registration failures are
/// swallowed here too, matching every existing call site
/// (`tm ticket audit`/`create`): a broken runs DB or marker directory must
/// never fail this command, since registration is pure telemetry.
pub fn register(store: &RunStore, sessions_dir: &Path, env: &SessionEnv, kind: &str, key: &str) {
    let ticket = key.to_uppercase();
    let _ = register_session(store, sessions_dir, env, kind, &ticket);
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
            kind: "lane".to_string(),
            log_path: None,
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
    fn finish_stores_valid_model_usage_json_verbatim_for_an_unpriced_model() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        let mut out = Vec::new();

        finish(
            &store,
            id,
            &FinishRun {
                status: RunStatus::Done,
                model_usage: Some(r#"{"claude-unpriced-model":{"inputTokens":146}}"#.to_string()),
                ..FinishRun::default()
            },
            &mut out,
        )
        .expect("should succeed");

        let run = store.run_by_id(id).unwrap().expect("expected a run");
        let usage = crate::runs::parse_model_usage(run.model_usage.as_deref().unwrap())
            .expect("stored model_usage should parse");
        let unpriced = &usage["claude-unpriced-model"];
        assert_eq!(unpriced.input_tokens, 146);
        assert_eq!(
            unpriced.cost_usd, None,
            "a model absent from the price table gets no estimate injected"
        );
        assert!(!unpriced.estimated);
    }

    #[test]
    fn finish_estimates_missing_cost_for_a_priced_model_and_marks_it_estimated() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        let mut out = Vec::new();

        finish(
            &store,
            id,
            &FinishRun {
                status: RunStatus::Done,
                model_usage: Some(
                    r#"{"claude-sonnet-5":{"inputTokens":1000000,"outputTokens":0,"cacheReadInputTokens":0,"cacheCreationInputTokens":0}}"#
                        .to_string(),
                ),
                ..FinishRun::default()
            },
            &mut out,
        )
        .expect("should succeed");

        let run = store.run_by_id(id).unwrap().expect("expected a run");
        let usage = crate::runs::parse_model_usage(run.model_usage.as_deref().unwrap())
            .expect("stored model_usage should parse");
        let sonnet = &usage["claude-sonnet-5"];
        assert_eq!(sonnet.cost_usd, Some(3.00));
        assert!(sonnet.estimated);
        assert_eq!(
            run.cost_usd,
            Some(3.00),
            "finish should roll the estimated cost up into runs.cost_usd when none was given explicitly"
        );
    }

    #[test]
    fn finish_never_overwrites_an_explicitly_given_cost_usd_with_the_rollup() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        let mut out = Vec::new();

        finish(
            &store,
            id,
            &FinishRun {
                status: RunStatus::Done,
                cost_usd: Some(42.0),
                model_usage: Some(
                    r#"{"claude-sonnet-5":{"inputTokens":1000000,"outputTokens":0,"cacheReadInputTokens":0,"cacheCreationInputTokens":0}}"#
                        .to_string(),
                ),
                ..FinishRun::default()
            },
            &mut out,
        )
        .expect("should succeed");

        let run = store.run_by_id(id).unwrap().expect("expected a run");
        assert_eq!(run.cost_usd, Some(42.0));
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
        list(&store, None, &mut list_out).unwrap();
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
        list(&store, None, &mut list_out).unwrap();
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

        let err =
            event(&store, id, "agent_usage", Some("not json"), &mut out).expect_err("should fail");

        assert!(matches!(err, RunsCliError::InvalidDetailJson(_)));
        assert!(out.is_empty());

        let mut list_out = Vec::new();
        list(&store, None, &mut list_out).unwrap();
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

        list(&store, None, &mut out).expect("should succeed");

        assert_eq!(String::from_utf8(out).unwrap(), "No runs recorded.\n");
    }

    #[test]
    fn list_by_outcome_prints_a_row_per_bucket_including_empty_ones() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let mut out = Vec::new();

        list_by_outcome(&store, None, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("not measured"), "got: {output:?}");
        assert!(output.contains("clean"), "got: {output:?}");
        assert!(output.contains("findings"), "got: {output:?}");
    }

    #[test]
    fn list_by_outcome_reports_run_counts_and_total_cost_per_bucket() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let clean_id = store.start_run(&start_params("PROJ-1")).unwrap();
        store
            .finish_run(
                clean_id,
                &FinishRun {
                    status: RunStatus::Done,
                    cost_usd: Some(2.5),
                    findings_count: Some(0),
                    ..FinishRun::default()
                },
            )
            .unwrap();

        let dirty_id = store.start_run(&start_params("PROJ-2")).unwrap();
        store
            .finish_run(
                dirty_id,
                &FinishRun {
                    status: RunStatus::Review,
                    cost_usd: Some(4.0),
                    findings_count: Some(2),
                    ..FinishRun::default()
                },
            )
            .unwrap();

        let unmeasured_id = store.start_run(&start_params("PROJ-3")).unwrap();
        store
            .finish_run(
                unmeasured_id,
                &FinishRun {
                    status: RunStatus::Done,
                    cost_usd: Some(1.0),
                    ..FinishRun::default()
                },
            )
            .unwrap();

        let mut out = Vec::new();
        list_by_outcome(&store, None, &mut out).expect("should succeed");
        let output = String::from_utf8(out).unwrap();

        let clean_line = output
            .lines()
            .find(|l| l.starts_with("clean"))
            .expect("expected a clean row");
        assert!(clean_line.contains('1'), "got: {clean_line:?}");
        assert!(clean_line.contains("2.50"), "got: {clean_line:?}");

        let findings_line = output
            .lines()
            .find(|l| l.starts_with("findings"))
            .expect("expected a findings row");
        assert!(findings_line.contains('1'), "got: {findings_line:?}");
        assert!(findings_line.contains("4.00"), "got: {findings_line:?}");

        let not_measured_line = output
            .lines()
            .find(|l| l.starts_with("not measured"))
            .expect("expected a not measured row");
        assert!(
            not_measured_line.contains('1'),
            "got: {not_measured_line:?}"
        );
        assert!(
            not_measured_line.contains("1.00"),
            "got: {not_measured_line:?}"
        );
    }

    #[test]
    fn list_by_outcome_renders_dash_for_a_bucket_with_no_cost_data() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let mut out = Vec::new();

        list_by_outcome(&store, None, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        let clean_line = output
            .lines()
            .find(|l| l.starts_with("clean"))
            .expect("expected a clean row");
        assert!(clean_line.contains('-'), "got: {clean_line:?}");
    }

    #[test]
    fn list_by_retro_prints_a_row_per_bucket_including_empty_ones() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let mut out = Vec::new();

        list_by_retro(&store, None, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("clean"), "got: {output:?}");
        assert!(output.contains("defect"), "got: {output:?}");
    }

    #[test]
    fn list_by_retro_reports_ticket_and_run_costs_and_excludes_ticket_with_no_run() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let clean_id = store.start_run(&start_params("PROJ-1")).unwrap();
        store
            .finish_run(
                clean_id,
                &FinishRun {
                    status: RunStatus::Done,
                    cost_usd: Some(5.0),
                    ..FinishRun::default()
                },
            )
            .unwrap();
        store
            .record_retro("PROJ-1", crate::runs::RetroVerdict::Clean, None, None)
            .unwrap();

        let defect_id = store.start_run(&start_params("PROJ-2")).unwrap();
        store
            .finish_run(
                defect_id,
                &FinishRun {
                    status: RunStatus::Done,
                    cost_usd: Some(10.0),
                    ..FinishRun::default()
                },
            )
            .unwrap();
        store
            .record_retro(
                "PROJ-2",
                crate::runs::RetroVerdict::Defect,
                Some(crate::runs::RetroSeverity::Major),
                None,
            )
            .unwrap();

        // PROJ-3 shipped a defect but has no recorded run at all.
        store
            .record_retro(
                "PROJ-3",
                crate::runs::RetroVerdict::Defect,
                Some(crate::runs::RetroSeverity::Minor),
                None,
            )
            .unwrap();

        let mut out = Vec::new();
        list_by_retro(&store, None, &mut out).expect("should succeed");
        let output = String::from_utf8(out).unwrap();

        let clean_line = output
            .lines()
            .find(|l| l.starts_with("clean"))
            .expect("expected a clean row");
        assert!(clean_line.contains("5.00"), "got: {clean_line:?}");

        let defect_line = output
            .lines()
            .find(|l| l.starts_with("defect"))
            .expect("expected a defect row");
        assert!(defect_line.contains("10.00"), "got: {defect_line:?}");
        assert!(
            !defect_line.contains("20.00"),
            "PROJ-3's missing run must not be counted as $0: {defect_line:?}"
        );
    }

    #[test]
    fn list_by_retro_respects_kind_filter() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let lane_id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "backend".to_string(),
                worktree: "/tmp/wt".to_string(),
                branch: None,
                pid: None,
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();
        store
            .finish_run(
                lane_id,
                &FinishRun {
                    status: RunStatus::Done,
                    cost_usd: Some(5.0),
                    ..FinishRun::default()
                },
            )
            .unwrap();

        let audit_id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "audit".to_string(),
                worktree: "/tmp/wt2".to_string(),
                branch: None,
                pid: None,
                kind: "audit".to_string(),
                log_path: None,
            })
            .unwrap();
        store
            .finish_run(
                audit_id,
                &FinishRun {
                    status: RunStatus::Done,
                    cost_usd: Some(1.0),
                    ..FinishRun::default()
                },
            )
            .unwrap();

        store
            .record_retro("PROJ-1", crate::runs::RetroVerdict::Clean, None, None)
            .unwrap();

        let mut out = Vec::new();
        list_by_retro(&store, Some("lane"), &mut out).expect("should succeed");
        let output = String::from_utf8(out).unwrap();

        let clean_line = output
            .lines()
            .find(|l| l.starts_with("clean"))
            .expect("expected a clean row");
        assert!(clean_line.contains("5.00"), "got: {clean_line:?}");
        assert!(!clean_line.contains("1.00"), "got: {clean_line:?}");
    }

    #[test]
    fn list_prints_header_and_row_for_a_run_with_no_events() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        store.start_run(&start_params("PROJ-1")).unwrap();
        let mut out = Vec::new();

        list(&store, None, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        let mut lines = output.lines();
        assert_eq!(
            lines.next(),
            Some("TICKET  LANE     KIND  STATUS   AGE  LAST EVENT")
        );
        let row = lines.next().expect("should have a data row");
        assert_eq!(row, "PROJ-1  backend  lane  running  0s   -");
        assert!(lines.next().is_none());
    }

    #[test]
    fn list_kind_filter_restricts_rows() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        store.start_run(&start_params("PROJ-1")).unwrap();
        store
            .start_run(&StartRun {
                kind: "audit".to_string(),
                ..start_params("PROJ-2")
            })
            .unwrap();
        let mut out = Vec::new();

        list(&store, Some("audit"), &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("PROJ-2"));
        assert!(!output.contains("PROJ-1"));
    }

    #[test]
    fn last_event_column_formats_kind_and_age_when_present() {
        let run = RunSummary {
            id: 1,
            ticket: "PROJ-1".to_string(),
            lane: "backend".to_string(),
            kind: "lane".to_string(),
            status: RunStatus::Running,
            age_secs: 120,
            heartbeat_age_secs: Some(5),
            last_event_kind: Some("tool_use".to_string()),
            last_event_age_secs: Some(45),
            awaiting_input: false,
        };

        assert_eq!(last_event_column(&run), "tool_use 45s ago");
    }

    #[test]
    fn last_event_column_is_dash_when_absent() {
        let run = RunSummary {
            id: 1,
            ticket: "PROJ-1".to_string(),
            lane: "backend".to_string(),
            kind: "lane".to_string(),
            status: RunStatus::Running,
            age_secs: 120,
            heartbeat_age_secs: Some(5),
            last_event_kind: None,
            last_event_age_secs: None,
            awaiting_input: false,
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
        show(&store, "proj-1", None, false, &mut out).expect("should succeed");
        let output = String::from_utf8(out).unwrap();

        assert!(output.starts_with(&format!("Run {id}: PROJ-1 [backend/lane] done\n")));
        assert!(output.contains("session sess-abc\n"));
        assert!(output.contains("pr https://example.invalid/pr/1\n"));
        assert!(output.contains("cost $1.50 / 3 turns\n"));
        assert!(output.contains("tool_use  {\"file\":\"a.rs\"}"));
        assert!(output.contains("  stop\n") || output.ends_with("  stop\n"));
        assert!(!output.contains("blocker"));
    }

    #[test]
    fn show_renders_not_measured_when_findings_count_is_null() {
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
        show(&store, "proj-1", None, false, &mut out).expect("should succeed");
        let output = String::from_utf8(out).unwrap();

        assert!(
            output.contains("findings not measured\n"),
            "got: {output:?}"
        );
    }

    #[test]
    fn show_renders_zero_findings_as_clean_not_bare_zero() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        store
            .finish_run(
                id,
                &FinishRun {
                    status: RunStatus::Done,
                    findings_count: Some(0),
                    ..FinishRun::default()
                },
            )
            .unwrap();

        let mut out = Vec::new();
        show(&store, "proj-1", None, false, &mut out).expect("should succeed");
        let output = String::from_utf8(out).unwrap();

        assert!(output.contains("findings 0 (clean)\n"), "got: {output:?}");
    }

    #[test]
    fn show_renders_a_nonzero_findings_count() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        store
            .finish_run(
                id,
                &FinishRun {
                    status: RunStatus::Review,
                    findings_count: Some(4),
                    ..FinishRun::default()
                },
            )
            .unwrap();

        let mut out = Vec::new();
        show(&store, "proj-1", None, false, &mut out).expect("should succeed");
        let output = String::from_utf8(out).unwrap();

        assert!(output.contains("findings 4\n"), "got: {output:?}");
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
        show(&store, "PROJ-1", None, false, &mut out).expect("should succeed");
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
        show(&store, "PROJ-1", None, false, &mut out).expect("should succeed");
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
        show(&store, "PROJ-1", None, false, &mut out).expect("should succeed");
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
        show(&store, "PROJ-1", None, false, &mut out).expect("should succeed");
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
        show(&store, "PROJ-1", None, false, &mut out).expect("should succeed");
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
        show(&store, "PROJ-1", None, false, &mut out).expect("should succeed");
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
        show(&store, "PROJ-1", None, false, &mut out).expect("should succeed");
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
        show(&store, "PROJ-1", None, false, &mut out).expect("should succeed");
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
        show(&store, "PROJ-1", None, false, &mut out).expect("should succeed");
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
        show(&store, "PROJ-1", None, false, &mut out).expect("should succeed");
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
        show(&store, "PROJ-1", None, false, &mut out).expect("should succeed");
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
        show(&store, "PROJ-1", None, false, &mut out).expect("should succeed");
        let output = String::from_utf8(out).unwrap();

        assert!(!output.contains("Agent usage"));
    }

    #[test]
    fn show_with_no_events_prints_placeholder() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        store.start_run(&start_params("PROJ-1")).unwrap();

        let mut out = Vec::new();
        show(&store, "PROJ-1", None, false, &mut out).expect("should succeed");
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

        let err = show(&store, "PROJ-404", None, false, &mut out).expect_err("should fail");

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

        let err = show(&store, "PROJ-404", None, true, &mut out).expect_err("should fail");

        assert!(matches!(
            err,
            RunsCliError::NoRunForTicket { ticket } if ticket == "PROJ-404"
        ));
        assert!(out.is_empty());
    }

    #[test]
    fn show_kind_filter_disambiguates_shadowing_run() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        store.start_run(&start_params("PROJ-1")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        store
            .start_run(&StartRun {
                kind: "audit".to_string(),
                ..start_params("PROJ-1")
            })
            .unwrap();
        let mut out = Vec::new();

        show(&store, "PROJ-1", Some("lane"), false, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("[backend/lane]"));
    }

    #[test]
    fn show_kind_filter_errors_when_no_run_of_that_kind() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        store.start_run(&start_params("PROJ-1")).unwrap();
        let mut out = Vec::new();

        let err = show(&store, "PROJ-1", Some("audit"), false, &mut out).expect_err("should fail");

        assert!(matches!(
            err,
            RunsCliError::NoRunForTicket { ticket } if ticket == "PROJ-1"
        ));
    }

    #[test]
    fn show_json_kind_filter_disambiguates_shadowing_run() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        store.start_run(&start_params("PROJ-1")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        store
            .start_run(&StartRun {
                kind: "audit".to_string(),
                ..start_params("PROJ-1")
            })
            .unwrap();
        let mut out = Vec::new();

        show(&store, "PROJ-1", Some("lane"), true, &mut out).expect("should succeed");

        let value: serde_json::Value = serde_json::from_str(&String::from_utf8(out).unwrap())
            .expect("output should be valid JSON");
        assert_eq!(value["run"]["kind"], "lane");
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
        show(&store, "proj-1", None, true, &mut out).expect("should succeed");
        let output = String::from_utf8(out).unwrap();

        let value: serde_json::Value =
            serde_json::from_str(&output).expect("output should be valid JSON");

        assert_eq!(value["run"]["id"], id);
        assert_eq!(value["run"]["ticket"], "PROJ-1");
        assert_eq!(value["run"]["lane"], "backend");
        assert_eq!(value["run"]["kind"], "lane");
        assert_eq!(value["run"]["status"], "done");
        assert_eq!(value["run"]["session_id"], "sess-abc");
        assert_eq!(value["run"]["worktree"], "/tmp/wt");
        assert_eq!(value["run"]["branch"], serde_json::Value::Null);
        assert_eq!(value["run"]["pid"], serde_json::Value::Null);
        assert_eq!(value["run"]["cost_usd"], 1.5);
        assert_eq!(value["run"]["num_turns"], 3);
        assert_eq!(value["run"]["pr_url"], "https://example.invalid/pr/1");
        assert_eq!(value["run"]["blocker"], serde_json::Value::Null);
        assert_eq!(value["run"]["findings_count"], serde_json::Value::Null);
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
    fn show_json_distinguishes_zero_findings_from_null() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        store
            .finish_run(
                id,
                &FinishRun {
                    status: RunStatus::Done,
                    findings_count: Some(0),
                    ..FinishRun::default()
                },
            )
            .unwrap();

        let mut out = Vec::new();
        show(&store, "proj-1", None, true, &mut out).expect("should succeed");
        let value: serde_json::Value = serde_json::from_str(&String::from_utf8(out).unwrap())
            .expect("output should be valid JSON");

        assert_eq!(value["run"]["findings_count"], 0);
    }

    #[test]
    fn show_json_checklist_and_model_usage_are_null_when_absent() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        store.start_run(&start_params("PROJ-1")).unwrap();

        let mut out = Vec::new();
        show(&store, "PROJ-1", None, true, &mut out).expect("should succeed");
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
        show(&store, "PROJ-1", None, true, &mut out).expect("should succeed");
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
        show(&store, "PROJ-1", None, true, &mut out).expect("should succeed");
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
        show(&store, "PROJ-1", None, true, &mut out).expect("should succeed");
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
    fn show_json_model_usage_source_is_estimated_when_the_authoritative_column_holds_a_derived_cost()
     {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        finish(
            &store,
            id,
            &FinishRun {
                status: RunStatus::Done,
                model_usage: Some(
                    r#"{"claude-sonnet-5":{"inputTokens":1000000,"outputTokens":0,"cacheReadInputTokens":0,"cacheCreationInputTokens":0}}"#
                        .to_string(),
                ),
                ..FinishRun::default()
            },
            &mut Vec::new(),
        )
        .unwrap();

        let mut out = Vec::new();
        show(&store, "PROJ-1", None, true, &mut out).expect("should succeed");
        let value: serde_json::Value = serde_json::from_str(&String::from_utf8(out).unwrap())
            .expect("output should be valid JSON");

        assert_eq!(value["model_usage"]["source"], "estimated");
        assert_eq!(
            value["model_usage"]["models"]["claude-sonnet-5"]["costUSD"],
            3.00
        );
        assert_eq!(
            value["model_usage"]["models"]["claude-sonnet-5"]["estimated"],
            true
        );
    }

    #[test]
    fn show_marks_an_estimated_cost_with_a_tilde_and_labels_the_usage_section() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = store.start_run(&start_params("PROJ-1")).unwrap();
        finish(
            &store,
            id,
            &FinishRun {
                status: RunStatus::Done,
                model_usage: Some(
                    r#"{"claude-sonnet-5":{"inputTokens":1000000,"outputTokens":0,"cacheReadInputTokens":0,"cacheCreationInputTokens":0}}"#
                        .to_string(),
                ),
                ..FinishRun::default()
            },
            &mut Vec::new(),
        )
        .unwrap();

        let mut out = Vec::new();
        show(&store, "PROJ-1", None, false, &mut out).expect("should succeed");
        let output = String::from_utf8(out).unwrap();

        assert!(
            output.contains("cost ~$3.00"),
            "estimated cost must be tilde-marked in the header: {output}"
        );
        assert!(
            output.contains("Model usage (estimated)"),
            "estimated usage section must say so: {output}"
        );
    }

    #[test]
    fn show_json_agent_usage_is_empty_array_when_absent() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        store.start_run(&start_params("PROJ-1")).unwrap();

        let mut out = Vec::new();
        show(&store, "PROJ-1", None, true, &mut out).expect("should succeed");
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
        show(&store, "PROJ-1", None, true, &mut out).expect("should succeed");
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
        show(&store, "PROJ-1", None, true, &mut out).expect("should succeed");
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
        show(&store, "PROJ-1", None, false, &mut human_out).expect("should succeed");
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
        show(&store, "PROJ-1", None, true, &mut out).expect("should succeed");
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
        let mut stderr = Vec::new();
        resume(&store, "proj-1", &mut out, &mut stderr).expect("should succeed");

        assert_eq!(String::from_utf8(out).unwrap(), "sess-abc\n");
    }

    #[test]
    fn resume_warns_on_stderr_when_run_is_terminal() {
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
        let mut stderr = Vec::new();
        resume(&store, "proj-1", &mut out, &mut stderr).expect("should succeed");

        let warning = String::from_utf8(stderr).unwrap();
        assert!(warning.contains("PROJ-1"));
        assert!(warning.contains("done"));
        assert!(warning.contains("tm runs reopen"));
        // Still resumable -- the warning must not have touched stdout.
        assert_eq!(String::from_utf8(out).unwrap(), "sess-abc\n");
    }

    #[test]
    fn resume_does_not_warn_when_run_is_running() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        store
            .update_session_id(
                store.start_run(&start_params("PROJ-1")).unwrap(),
                "sess-live",
            )
            .unwrap();

        let mut out = Vec::new();
        let mut stderr = Vec::new();
        resume(&store, "proj-1", &mut out, &mut stderr).expect("should succeed");

        assert!(stderr.is_empty());
        assert_eq!(String::from_utf8(out).unwrap(), "sess-live\n");
    }

    #[test]
    fn resume_unknown_ticket_errors() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let mut out = Vec::new();
        let mut stderr = Vec::new();

        let err = resume(&store, "PROJ-404", &mut out, &mut stderr).expect_err("should fail");

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
        let mut stderr = Vec::new();
        let err = resume(&store, "PROJ-1", &mut out, &mut stderr).expect_err("should fail");

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

    // --- reopen ---

    fn finished_run(store: &RunStore, ticket: &str, status: RunStatus) -> i64 {
        let id = store.start_run(&start_params(ticket)).unwrap();
        store
            .finish_run(
                id,
                &FinishRun {
                    status,
                    session_id: Some("sess-abc".to_string()),
                    ..FinishRun::default()
                },
            )
            .unwrap();
        id
    }

    #[test]
    fn reopen_by_ticket_moves_it_to_queued_by_default() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = finished_run(&store, "PROJ-1", RunStatus::Done);

        let mut out = Vec::new();
        reopen(&store, "proj-1", None, RunStatus::Queued, &mut out).expect("should succeed");

        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("Reopened run {id} (PROJ-1): done -> queued\n")
        );
        let run = store.run_by_id(id).unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Queued);
    }

    #[test]
    fn reopen_by_numeric_id_ignores_kind() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = finished_run(&store, "PROJ-1", RunStatus::Interrupted);

        let mut out = Vec::new();
        reopen(
            &store,
            &id.to_string(),
            Some("audit"),
            RunStatus::Running,
            &mut out,
        )
        .expect("should succeed");

        let run = store.run_by_id(id).unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Running);
    }

    #[test]
    fn reopen_errors_on_unknown_numeric_id() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let mut out = Vec::new();

        let err =
            reopen(&store, "999", None, RunStatus::Queued, &mut out).expect_err("should fail");

        assert!(matches!(err, RunsCliError::NoRunWithId(999)));
        assert!(out.is_empty());
    }

    #[test]
    fn reopen_errors_on_unknown_ticket() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let mut out = Vec::new();

        let err =
            reopen(&store, "PROJ-404", None, RunStatus::Queued, &mut out).expect_err("should fail");

        assert!(matches!(
            err,
            RunsCliError::NoRunForTicket { ticket } if ticket == "PROJ-404"
        ));
        assert!(out.is_empty());
    }

    #[test]
    fn reopen_errors_on_non_terminal_run() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        store.start_run(&start_params("PROJ-1")).unwrap(); // left running

        let mut out = Vec::new();
        let err =
            reopen(&store, "proj-1", None, RunStatus::Queued, &mut out).expect_err("should fail");

        assert!(matches!(
            err,
            RunsCliError::Store(RunStoreError::NotTerminal { .. })
        ));
        assert!(out.is_empty());
    }

    // --- logs ---

    struct NoopSleeper;
    impl crate::work::review_watch::Sleeper for NoopSleeper {
        fn sleep(&self, _secs: u64) {}
    }

    #[test]
    fn logs_prints_the_tail_of_the_recorded_log_path() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let log_file = dir.path().join("mylane.log");
        std::fs::write(&log_file, "line1\nline2\nline3\n").unwrap();
        store
            .start_run(&StartRun {
                log_path: Some(log_file.to_string_lossy().into_owned()),
                ..start_params("PROJ-1")
            })
            .unwrap();
        let mut out = Vec::new();

        logs(
            &store,
            dir.path(),
            "proj-1",
            None,
            2,
            false,
            &NoopSleeper,
            &mut out,
        )
        .expect("should succeed");

        assert_eq!(String::from_utf8(out).unwrap(), "line2\nline3\n");
    }

    #[test]
    fn logs_falls_back_to_the_review_watch_convention_when_log_path_is_null() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        store
            .start_run(&StartRun {
                ticket: "AX-408".to_string(),
                lane: "review-watch".to_string(),
                kind: "review-watch".to_string(),
                log_path: None,
                ..start_params("AX-408")
            })
            .unwrap();
        let expected_path = crate::cli::pr::watch_log_dir(dir.path()).join("ax-408.log");
        std::fs::create_dir_all(expected_path.parent().unwrap()).unwrap();
        std::fs::write(&expected_path, "watch_started\n").unwrap();
        let mut out = Vec::new();

        logs(
            &store,
            dir.path(),
            "ax-408",
            None,
            200,
            false,
            &NoopSleeper,
            &mut out,
        )
        .expect("should succeed");

        assert_eq!(String::from_utf8(out).unwrap(), "watch_started\n");
    }

    #[test]
    fn logs_reports_the_empty_case_distinctly_and_points_at_show() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let log_file = dir.path().join("ax-408.log");
        std::fs::write(&log_file, "").unwrap();
        let id = store
            .start_run(&StartRun {
                ticket: "AX-408".to_string(),
                log_path: Some(log_file.to_string_lossy().into_owned()),
                ..start_params("AX-408")
            })
            .unwrap();
        let mut out = Vec::new();

        logs(
            &store,
            dir.path(),
            "ax-408",
            None,
            200,
            false,
            &NoopSleeper,
            &mut out,
        )
        .expect("should succeed");

        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("is empty"), "got: {printed:?}");
        assert!(printed.contains("tm runs show AX-408"), "got: {printed:?}");
        assert!(printed.contains(&id.to_string()), "got: {printed:?}");
    }

    #[test]
    fn logs_errors_distinctly_when_no_log_path_and_no_fallback_for_kind() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                kind: "lane".to_string(),
                log_path: None,
                ..start_params("PROJ-1")
            })
            .unwrap();
        let mut out = Vec::new();

        let err = logs(
            &store,
            dir.path(),
            "proj-1",
            None,
            200,
            false,
            &NoopSleeper,
            &mut out,
        )
        .expect_err("should fail");

        assert!(matches!(err, RunsCliError::NoLogPath { .. }));
    }

    #[test]
    fn logs_errors_distinctly_when_the_resolved_file_is_missing() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        store
            .start_run(&StartRun {
                log_path: Some(dir.path().join("gone.log").to_string_lossy().into_owned()),
                ..start_params("PROJ-1")
            })
            .unwrap();
        let mut out = Vec::new();

        let err = logs(
            &store,
            dir.path(),
            "proj-1",
            None,
            200,
            false,
            &NoopSleeper,
            &mut out,
        )
        .expect_err("should fail");

        assert!(matches!(err, RunsCliError::LogFileMissing { .. }));
    }

    #[test]
    fn tail_lines_returns_all_when_shorter_than_n() {
        assert_eq!(tail_lines("a\nb\n", 5), vec!["a", "b"]);
    }

    #[test]
    fn tail_lines_returns_the_last_n_lines() {
        assert_eq!(tail_lines("a\nb\nc\nd\n", 2), vec!["c", "d"]);
    }

    #[test]
    fn tail_lines_zero_returns_empty() {
        assert!(tail_lines("a\nb\n", 0).is_empty());
    }

    #[test]
    fn read_appended_returns_only_bytes_written_after_the_offset() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("f.log");
        std::fs::write(&path, "first\n").unwrap();
        let offset = std::fs::metadata(&path).unwrap().len();
        std::fs::write(&path, "first\nsecond\n").unwrap();

        let (appended, new_offset) = read_appended(&path, offset).unwrap();

        assert_eq!(appended, "second\n");
        assert_eq!(new_offset, "first\nsecond\n".len() as u64);
    }

    #[test]
    fn read_appended_returns_empty_when_nothing_new() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("f.log");
        std::fs::write(&path, "first\n").unwrap();
        let offset = std::fs::metadata(&path).unwrap().len();

        let (appended, new_offset) = read_appended(&path, offset).unwrap();

        assert_eq!(appended, "");
        assert_eq!(new_offset, offset);
    }

    // --- register ---

    fn env_with_session(session_id: &str) -> SessionEnv {
        SessionEnv {
            session_id: Some(session_id.to_string()),
            claude_pid: Some(4242),
            lane_run_id: None,
            session_run_id: None,
            cwd: std::path::PathBuf::from("/tmp/wt"),
        }
    }

    #[test]
    fn register_is_a_noop_without_a_session_id() {
        let db_dir = tempdir().unwrap();
        let markers_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());
        let env = SessionEnv {
            session_id: None,
            claude_pid: None,
            lane_run_id: None,
            session_run_id: None,
            cwd: std::path::PathBuf::from("/tmp/wt"),
        };

        register(&store, markers_dir.path(), &env, "bugbot-cleanup", "PROJ-1");

        assert!(store.list_runs().unwrap().is_empty());
    }

    #[test]
    fn register_adopts_when_session_id_is_set() {
        let db_dir = tempdir().unwrap();
        let markers_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());
        let env = env_with_session("sess-1");

        register(&store, markers_dir.path(), &env, "bugbot-cleanup", "proj-1");

        let runs = store.list_runs().unwrap();
        assert_eq!(runs.len(), 1, "expected exactly one registered run");
        let run = &runs[0];
        assert_eq!(run.ticket, "PROJ-1", "key should be uppercased");
        assert_eq!(run.kind, "bugbot-cleanup");
        assert_eq!(run.status, RunStatus::Running);

        let marker = markers_dir.path().join("sess-1");
        assert!(marker.exists());
    }

    #[test]
    fn register_adopts_a_preregistered_run_matching_kind_and_ticket() {
        let db_dir = tempdir().unwrap();
        let markers_dir = tempdir().unwrap();
        let store = open_store(db_dir.path());

        let pre_id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "bugbot-cleanup".to_string(),
                worktree: "/repo/axiom".to_string(),
                branch: None,
                pid: None,
                kind: "bugbot-cleanup".to_string(),
                log_path: None,
            })
            .unwrap();

        let mut env = env_with_session("sess-1");
        env.session_run_id = Some(pre_id);

        register(&store, markers_dir.path(), &env, "bugbot-cleanup", "PROJ-1");

        assert_eq!(store.list_runs().unwrap().len(), 1, "no new run created");
        let run = store.run_by_id(pre_id).unwrap().expect("run row exists");
        assert_eq!(run.session_id, Some("sess-1".to_string()));
        assert_eq!(run.status, RunStatus::Running);
    }
}
