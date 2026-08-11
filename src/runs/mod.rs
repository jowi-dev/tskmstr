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

use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;

pub mod pid;
pub mod session;

/// A SQL expression yielding the current UTC time as
/// `YYYY-MM-DDTHH:MM:SS.sssZ`. Takes no user input, so it is safe to splice
/// directly into statement text; all user-supplied values still go through
/// bound `?` parameters.
const NOW_SQL: &str = "strftime('%Y-%m-%dT%H:%M:%fZ','now')";

/// Schema migrations, indexed by `PRAGMA user_version`. `MIGRATIONS[0]` is
/// applied to take a fresh database from version 0 to version 1, and so on.
/// Future schema changes append here rather than editing existing entries.
const MIGRATIONS: &[&str] = &[
    r#"
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
    "#,
    r#"
    CREATE TABLE ticket_audits (
      id INTEGER PRIMARY KEY,
      ticket_key TEXT NOT NULL,
      verdict TEXT NOT NULL,
      notes TEXT,
      audited_at TEXT NOT NULL
    );
    CREATE INDEX idx_ticket_audits_key ON ticket_audits(ticket_key, audited_at);
    "#,
    r#"
    ALTER TABLE runs ADD COLUMN model_usage TEXT;
    "#,
    r#"
    ALTER TABLE runs ADD COLUMN kind TEXT NOT NULL DEFAULT 'lane';
    "#,
    r#"
    ALTER TABLE runs ADD COLUMN log_path TEXT;
    "#,
];

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

    /// [`RunStore::reopen_run`] was called on a run whose status isn't
    /// terminal (see [`RunStatus::is_terminal`]) — reopening only makes sense
    /// for a run that has already finished.
    #[error(
        "run {id} is not in a terminal state (status: {status}); only done/failed/interrupted runs can be reopened"
    )]
    NotTerminal {
        /// Row id that was rejected.
        id: i64,
        /// Its current (non-terminal) status string.
        status: String,
    },
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
    /// Finished with an error: the agent ran and concluded it failed (a
    /// non-zero `claude` exit, or an explicit `is_error: true` in its result
    /// JSON).
    Failed,
    /// Ended abnormally, or its outcome could not be determined — distinct
    /// from [`RunStatus::Failed`]. `Failed` means the agent tried and
    /// reported failure; `Interrupted` means the run's terminal JSON was
    /// unparseable, or the `is_error` field was entirely absent (rather than
    /// explicitly `false`), which is exactly the shape a mid-run event like a
    /// usage-limit model switch can produce. Treating that as `Done` is the
    /// bug this variant exists to avoid — see
    /// [`crate::work::runner::parse_run_outcome`]'s doc comment. Terminal for
    /// board/ordering purposes, same as `Done`/`Failed`.
    Interrupted,
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
            RunStatus::Interrupted => "interrupted",
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
            "interrupted" => Some(RunStatus::Interrupted),
            _ => None,
        }
    }

    /// Whether this status is terminal: the run has finished (successfully
    /// or not) and isn't going to progress on its own. Used by [`RunStore`]'s
    /// list ordering and by `tm runs reopen`'s precondition — a run must be
    /// terminal before it can be reopened.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            RunStatus::Done | RunStatus::Failed | RunStatus::Interrupted
        )
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
    /// Discriminates what kind of run this is, e.g. `lane`, `audit`,
    /// `create` (see `docs/plans/session-usage.md`). Free-form text at this
    /// layer, same stance as `lane` and event `kind`; existing `tm work run`
    /// callers pass `"lane"`.
    pub kind: String,
    /// Path of the log file this run's detached process (if any) writes its
    /// stdout/stderr to, e.g. `~/.local/state/tskmstr/review-watch/proj-1.log`.
    /// `None` for runs with no detached process (interactive sessions,
    /// `tm work run --fg`) or wherever the caller doesn't yet know the path
    /// at `start_run` time — see [`RunStore::update_log_path`] for that case.
    pub log_path: Option<String>,
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
    /// Per-model token/cost usage, as a JSON object string keyed by model
    /// name (see [`ModelUsage`]). Verbatim from `claude -p`'s `modelUsage`
    /// map. Validated as JSON by the CLI layer before it reaches here.
    pub model_usage: Option<String>,
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
            model_usage: None,
        }
    }
}

/// A run reopened by [`RunStore::reopen_run`].
#[derive(Debug, Clone)]
pub struct ReopenedRun {
    /// Row id.
    pub id: i64,
    /// Jira ticket key.
    pub ticket: String,
    /// The status it had before being reopened.
    pub old_status: RunStatus,
    /// The status it was moved to.
    pub new_status: RunStatus,
}

/// A run marked failed by [`RunStore::reap`].
#[derive(Debug, Clone)]
pub struct ReapedRun {
    /// Row id.
    pub id: i64,
    /// Jira ticket key.
    pub ticket: String,
    /// PID recorded for the run, if any.
    pub pid: Option<u32>,
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
    /// Discriminates what kind of run this is; see [`StartRun::kind`].
    pub kind: String,
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
    /// Whether the run is currently awaiting user input; see
    /// [`is_awaiting_input`].
    pub awaiting_input: bool,
}

/// Derives whether a run is currently awaiting user input: `status` is
/// [`RunStatus::Running`] and its most recent event is `await` (emitted by
/// `hooks/tm-session-state.sh` on `Stop`/`Notification` for a registered
/// interactive session). This is a pure, read-side derivation — no schema
/// change and no new [`RunStatus`] variant (ADR-0001) — so any later event
/// with a different `kind` (`resume` from `UserPromptSubmit`, or a plain
/// `tool`/`usage` event) flips it back to `false` by replacing
/// `last_event_kind`, and a run that has already finished is never
/// "awaiting" even if `await` happened to be its last recorded event.
pub fn is_awaiting_input(status: RunStatus, last_event_kind: Option<&str>) -> bool {
    status == RunStatus::Running && last_event_kind == Some("await")
}

/// Full row from `runs`, used by `tm runs show`/`resume` and (later) the
/// watch detail view.
#[derive(Debug, Clone)]
pub struct Run {
    /// Row id.
    pub id: i64,
    /// Jira ticket key.
    pub ticket: String,
    /// Lane name.
    pub lane: String,
    /// Discriminates what kind of run this is; see [`StartRun::kind`].
    pub kind: String,
    /// Current status.
    pub status: RunStatus,
    /// `claude -p` session id, if recorded.
    pub session_id: Option<String>,
    /// Filesystem path of the git worktree the run used.
    pub worktree: String,
    /// Branch checked out in the worktree, if known.
    pub branch: Option<String>,
    /// PID of the runner process, if known.
    pub pid: Option<u32>,
    /// Filesystem path of the full transcript, if one was captured.
    pub transcript: Option<String>,
    /// When the run started.
    pub started_at: String,
    /// Last heartbeat time, if any.
    pub heartbeat_at: Option<String>,
    /// When the run ended, if it has.
    pub ended_at: Option<String>,
    /// Process exit code, if the run exited normally.
    pub exit_code: Option<i32>,
    /// Number of turns the run took, if known.
    pub num_turns: Option<i64>,
    /// Reported cost of the run in USD, if known.
    pub cost_usd: Option<f64>,
    /// Escalation text, set when `status` is [`RunStatus::Blocked`].
    pub blocker: Option<String>,
    /// URL of the pull request the run opened, if any.
    pub pr_url: Option<String>,
    /// Seconds since `started_at`, computed in SQL like [`RunSummary::age_secs`].
    pub age_secs: i64,
    /// Per-model token/cost usage recorded at `finish`, as raw JSON (see
    /// [`FinishRun::model_usage`]), if known. Parse with
    /// [`parse_model_usage`].
    pub model_usage: Option<String>,
    /// Path of this run's detached-process log file, if recorded; see
    /// [`StartRun::log_path`]. `None` for runs with no detached process, and
    /// for any run started before this column existed — `tm runs logs`
    /// falls back to the by-convention path for `kind` in that case.
    pub log_path: Option<String>,
}

/// A recorded audit verdict for a ticket, from [`RunStore::record_audit`]
/// and [`RunStore::latest_audit_for_ticket`].
#[derive(Debug, Clone)]
pub struct TicketAudit {
    /// Jira ticket key the audit was recorded for.
    pub ticket_key: String,
    /// Audit verdict, e.g. `ready` or `needs-work`.
    pub verdict: String,
    /// Optional free-text notes attached to the verdict.
    pub notes: Option<String>,
    /// When the audit was recorded, per [`NOW_SQL`].
    pub audited_at: String,
}

/// One `run_events` row.
#[derive(Debug, Clone)]
pub struct RunEvent {
    /// Row id.
    pub id: i64,
    /// When the event was recorded.
    pub at: String,
    /// Event kind, e.g. `tool_use` or `stop`.
    pub kind: String,
    /// Optional detail payload, stored as-is.
    pub detail: Option<String>,
}

/// One item in a run's checklist snapshot.
///
/// Mirrors the shape a runner emits via
/// `tm runs event <ID> --kind checklist --detail '{"items":[...]}'`: see
/// [`latest_checklist`] for the full convention. Unknown extra JSON fields
/// on an item are ignored rather than rejected, so the convention can grow
/// without breaking older `tm` builds.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct ChecklistItem {
    /// The checklist item's text, e.g. `"write tests"`.
    pub text: String,
    /// Whether the item is complete.
    pub done: bool,
}

/// A run's checklist, as of its most recent `checklist` event.
///
/// Every `checklist` event carries a full snapshot rather than a diff
/// against the previous one, so this is simply the parsed detail of the
/// newest event that parsed successfully; see [`latest_checklist`].
#[derive(Debug, Clone, PartialEq)]
pub struct ChecklistState {
    /// Checklist items, in emitted order.
    pub items: Vec<ChecklistItem>,
}

impl ChecklistState {
    /// Number of items marked done.
    pub fn done_count(&self) -> usize {
        self.items.iter().filter(|item| item.done).count()
    }
}

/// The JSON shape of a `checklist` event's `detail`: `{"items":[{"text":
/// "...","done":false}, ...]}`. Kept private; callers only ever see the
/// parsed [`ChecklistState`].
#[derive(serde::Deserialize)]
struct ChecklistDetail {
    items: Vec<ChecklistItem>,
}

/// Scans `events` (as returned by [`RunStore::events_for_run`], oldest
/// first) for the newest event with `kind == "checklist"` whose `detail`
/// parses as the documented shape, and returns it.
///
/// This is the convention a Claude Code "lane" run uses to report
/// fine-grained progress: `tm runs event <ID> --kind checklist --detail
/// '{"items":[{"text":"write tests","done":true},{"text":"implement",
/// "done":false}]}'`. Each `checklist` event is a full snapshot — latest
/// wins, there is no diffing against earlier ones.
///
/// Tolerant by design: an event with kind `checklist` but missing or
/// malformed `detail` (not valid JSON, or valid JSON that doesn't match the
/// shape) is skipped in favor of the next-newest `checklist` event that does
/// parse, rather than erroring or panicking. Returns `None` if no event
/// parses.
pub fn latest_checklist(events: &[RunEvent]) -> Option<ChecklistState> {
    events
        .iter()
        .rev()
        .filter(|event| event.kind == "checklist")
        .find_map(|event| {
            let detail = event.detail.as_deref()?;
            let parsed: ChecklistDetail = serde_json::from_str(detail).ok()?;
            Some(ChecklistState {
                items: parsed.items,
            })
        })
}

/// Per-model token/cost usage, as reported verbatim in a `claude -p`
/// result's `modelUsage` map. All token fields default to `0` when absent
/// so a live snapshot (which never carries `costUSD`) still parses cleanly.
#[derive(Debug, Clone, Copy, PartialEq, Default, serde::Deserialize, serde::Serialize)]
pub struct ModelUsage {
    /// Input tokens (excluding cache reads/writes).
    #[serde(rename = "inputTokens", default)]
    pub input_tokens: u64,
    /// Output tokens generated.
    #[serde(rename = "outputTokens", default)]
    pub output_tokens: u64,
    /// Tokens read from the prompt cache.
    #[serde(rename = "cacheReadInputTokens", default)]
    pub cache_read_input_tokens: u64,
    /// Tokens written to the prompt cache.
    #[serde(rename = "cacheCreationInputTokens", default)]
    pub cache_creation_input_tokens: u64,
    /// Cost in USD, present in the authoritative `runs.model_usage` column
    /// (recorded at `tm runs finish --model-usage`) and absent from live
    /// `usage` event snapshots.
    #[serde(rename = "costUSD", default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

/// A model name (e.g. `claude-fable-5`) to its [`ModelUsage`]. A
/// [`std::collections::BTreeMap`] rather than a `HashMap` so
/// [`format_model_usage`] renders models in a deterministic order.
pub type ModelUsageMap = std::collections::BTreeMap<String, ModelUsage>;

/// The JSON shape of a `usage` event's `detail`: `{"models":{"<model>":
/// {...}}}`. Kept private; callers only see the parsed [`ModelUsageMap`].
#[derive(serde::Deserialize)]
struct UsageDetail {
    models: ModelUsageMap,
}

/// Scans `events` (as returned by [`RunStore::events_for_run`], oldest
/// first) for the newest event with `kind == "usage"` whose `detail` parses
/// as `{"models":{...}}`, and returns the parsed map.
///
/// This is the convention a Claude Code "lane" run's Stop hook uses to
/// report live per-model token usage: `tm runs event <ID> --kind usage
/// --detail '{"models":{"claude-fable-5":{"inputTokens":146,
/// "outputTokens":58564,"cacheReadInputTokens":6535803,
/// "cacheCreationInputTokens":203983}}}'`. Each `usage` event is a full
/// snapshot, not a diff — latest wins, same convention as
/// [`latest_checklist`], and garbage-tolerant in the same way: an event
/// with kind `usage` but missing or malformed `detail` is skipped in favor
/// of the next-newest one that does parse.
///
/// `costUSD` is never present on these live events (it's only known once
/// the run finishes); see [`RunStore::finish_run`]'s `model_usage` column
/// for the authoritative, cost-bearing snapshot.
pub fn latest_usage(events: &[RunEvent]) -> Option<ModelUsageMap> {
    events
        .iter()
        .rev()
        .filter(|event| event.kind == "usage")
        .find_map(|event| {
            let detail = event.detail.as_deref()?;
            let parsed: UsageDetail = serde_json::from_str(detail).ok()?;
            Some(parsed.models)
        })
}

/// Parses the `runs.model_usage` column: a bare JSON object mapping model
/// name to [`ModelUsage`] (no `"models"` wrapper, unlike [`latest_usage`]'s
/// event `detail`), as passed verbatim to `tm runs finish --model-usage`.
/// Returns `None` if `json` doesn't parse as that shape.
pub fn parse_model_usage(json: &str) -> Option<ModelUsageMap> {
    serde_json::from_str(json).ok()
}

/// One completed `Agent`/`Task` invocation, as reported by an `agent_usage`
/// event's `detail`: `{"agentType": "elixir-implementer", "description":
/// "...", "model": "claude-sonnet-5", "outputTokens": 1143, "inputTokens": 2,
/// "cacheReadInputTokens": 87519, "cacheCreationInputTokens": 3012,
/// "totalToolUseCount": 38, "durationMs": 193659}`.
///
/// Unlike `checklist`/`usage` events, each `agent_usage` event is a discrete,
/// finished unit of work rather than a mutable snapshot — see
/// [`collect_agent_usage`] for why every event is kept rather than only the
/// newest. The token fields are `#[serde(flatten)]`ed from [`ModelUsage`] so
/// this struct reuses its field naming (`inputTokens`/`outputTokens`/
/// `cacheReadInputTokens`/`cacheCreationInputTokens`) rather than duplicating
/// it; `ModelUsage::cost_usd` is always `None` here since no per-agent cost
/// is ever available (see `docs/plans/per-agent-usage.md`).
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct AgentUsageEvent {
    /// The subagent type invoked, e.g. `elixir-implementer` or `Explore`.
    #[serde(rename = "agentType")]
    pub agent_type: String,
    /// The free-text description passed to the `Agent`/`Task` call, if any.
    #[serde(default)]
    pub description: Option<String>,
    /// The model the agent resolved to, e.g. `claude-sonnet-5`.
    pub model: String,
    /// Token usage for this invocation.
    #[serde(flatten)]
    pub usage: ModelUsage,
    /// Total tool calls the agent made during this invocation.
    #[serde(rename = "totalToolUseCount", default)]
    pub total_tool_use_count: u64,
    /// Wall-clock duration of the invocation, in milliseconds.
    #[serde(rename = "durationMs", default)]
    pub duration_ms: u64,
}

/// Scans `events` (as returned by [`RunStore::events_for_run`], oldest
/// first) for every event with `kind == "agent_usage"` whose `detail` parses
/// as [`AgentUsageEvent`], and returns them **in the same oldest-first
/// order** — deliberately not named `latest_*` like [`latest_checklist`]/
/// [`latest_usage`]: each `agent_usage` event is a discrete, finished
/// invocation rather than a snapshot of a mutable total, so repeat
/// invocations of the same agent type must all be kept rather than
/// collapsed to the newest one.
///
/// Tolerant by design, same as [`latest_checklist`]: an event with kind
/// `agent_usage` but missing or malformed `detail` is skipped rather than
/// erroring. Returns an empty `Vec` if no event parses.
pub fn collect_agent_usage(events: &[RunEvent]) -> Vec<AgentUsageEvent> {
    events
        .iter()
        .filter(|event| event.kind == "agent_usage")
        .filter_map(|event| {
            let detail = event.detail.as_deref()?;
            serde_json::from_str(detail).ok()
        })
        .collect()
}

/// Summed usage for one `(agent_type, model)` pair, across every invocation
/// captured by [`collect_agent_usage`]. See [`aggregate_agent_usage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AgentUsageTotals {
    /// Number of `agent_usage` events folded into this total.
    pub invocations: u64,
    /// Summed output tokens.
    pub output_tokens: u64,
    /// Summed input tokens (excluding cache reads/writes).
    pub input_tokens: u64,
    /// Summed tokens read from the prompt cache.
    pub cache_read_input_tokens: u64,
    /// Summed tokens written to the prompt cache.
    pub cache_creation_input_tokens: u64,
    /// Summed tool calls across all invocations.
    pub total_tool_use_count: u64,
    /// Summed wall-clock duration, in milliseconds.
    pub duration_ms: u64,
}

/// Groups `events` by `(agent_type, model)` and sums their token/tool-use/
/// duration fields, counting invocations per group.
///
/// Keyed by the compound `(agent_type, model)` pair, not `agent_type` alone
/// — per `docs/plans/per-agent-usage.md`'s "cross-model agents" hard part,
/// an agent type (e.g. `general-purpose`) can resolve to different models
/// across invocations, and merging their tokens under one row would make a
/// cost-conscious read of "which agent is expensive" wrong. Returns an
/// empty map for empty input. Uses a [`std::collections::BTreeMap`] rather
/// than a `HashMap` so renderers get a deterministic iteration order, same
/// rationale as [`ModelUsageMap`].
pub fn aggregate_agent_usage(
    events: &[AgentUsageEvent],
) -> std::collections::BTreeMap<(String, String), AgentUsageTotals> {
    let mut totals: std::collections::BTreeMap<(String, String), AgentUsageTotals> =
        std::collections::BTreeMap::new();

    for event in events {
        let key = (event.agent_type.clone(), event.model.clone());
        let entry = totals.entry(key).or_default();
        entry.invocations += 1;
        entry.output_tokens += event.usage.output_tokens;
        entry.input_tokens += event.usage.input_tokens;
        entry.cache_read_input_tokens += event.usage.cache_read_input_tokens;
        entry.cache_creation_input_tokens += event.usage.cache_creation_input_tokens;
        entry.total_tool_use_count += event.total_tool_use_count;
        entry.duration_ms += event.duration_ms;
    }

    totals
}

/// Renders [`aggregate_agent_usage`]'s output as one line per `agent_type`,
/// mirroring [`format_model_usage`]'s column layout (padded name column
/// followed by a comma-separated detail string).
///
/// [`aggregate_agent_usage`] keys by the compound `(agent_type, model)` pair
/// (see its doc comment for why), but most agent types only ever resolve to
/// one model, so this collapses each `agent_type` to a single line when it
/// has exactly one model. Only when an `agent_type` spans more than one
/// model does it expand into one line per model, named `{agent_type}
/// ({model})` — see `docs/plans/per-agent-usage.md`'s "cross-model agents"
/// hard part.
///
/// Unlike [`format_model_usage`], there is no cost column and no trailing
/// `total` line: `model_usage` and `agent_usage` are overlapping slices of
/// the same underlying tokens (subagent usage is already folded into the
/// authoritative per-model total), not additive line items, so summing
/// agent rows and presenting a total would misrepresent the run's cost.
///
/// Returns an empty `Vec` for an empty map.
pub fn format_agent_usage(
    totals: &std::collections::BTreeMap<(String, String), AgentUsageTotals>,
) -> Vec<String> {
    if totals.is_empty() {
        return Vec::new();
    }

    // BTreeMap iteration order sorts by (agent_type, model), so entries for
    // the same agent_type are already adjacent — a single pass groups them.
    let mut groups: Vec<(&str, Vec<(&str, &AgentUsageTotals)>)> = Vec::new();
    for ((agent_type, model), agent_totals) in totals {
        match groups.last_mut() {
            Some((last_type, models)) if *last_type == agent_type => {
                models.push((model.as_str(), agent_totals));
            }
            _ => groups.push((agent_type.as_str(), vec![(model.as_str(), agent_totals)])),
        }
    }

    let rows: Vec<(String, &AgentUsageTotals)> = groups
        .into_iter()
        .flat_map(|(agent_type, models)| {
            if models.len() == 1 {
                let (_, agent_totals) = models[0];
                vec![(agent_type.to_string(), agent_totals)]
            } else {
                models
                    .into_iter()
                    .map(|(model, agent_totals)| (format!("{agent_type} ({model})"), agent_totals))
                    .collect()
            }
        })
        .collect();

    let name_w = rows
        .iter()
        .map(|(name, _)| name.chars().count())
        .max()
        .unwrap_or(0)
        + 2;

    rows.into_iter()
        .map(|(name, agent_totals)| {
            let detail = format!(
                "{}x, out {}, in {}, cache-read {}, cache-write {}, tools {}",
                agent_totals.invocations,
                format_token_count(agent_totals.output_tokens),
                format_token_count(agent_totals.input_tokens),
                format_token_count(agent_totals.cache_read_input_tokens),
                format_token_count(agent_totals.cache_creation_input_tokens),
                agent_totals.total_tool_use_count,
            );
            format!("{name:<name_w$}{detail}")
        })
        .collect()
}

/// Formats a token count human-readably: under 1,000 as a plain integer,
/// under 1,000,000 as `{n.n}k`, otherwise as `{n.n}M`. E.g. `58564` ->
/// `"58.6k"`, `6535803` -> `"6.5M"`.
fn format_token_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Renders [`latest_usage`]/[`parse_model_usage`]'s output as one line per
/// model, plus a trailing `total` line when any model carries a `costUSD`
/// (i.e. the map came from the authoritative `runs.model_usage` column
/// rather than a live event snapshot). Cache tokens are always shown
/// alongside input/output — they dominate real cost on a cached-heavy run
/// and hiding them would misrepresent it.
///
/// Returns an empty `Vec` for an empty map.
pub fn format_model_usage(map: &ModelUsageMap) -> Vec<String> {
    if map.is_empty() {
        return Vec::new();
    }

    let any_cost = map.values().any(|usage| usage.cost_usd.is_some());
    let name_width = map
        .keys()
        .map(|name| name.chars().count())
        .max()
        .unwrap_or(0);
    let cost_width = map
        .values()
        .filter_map(|usage| usage.cost_usd)
        .map(|cost| format!("${cost:.2}").chars().count())
        .max()
        .unwrap_or(0);
    let name_w = name_width + 2;
    let cost_w = cost_width + 2;

    let mut lines = Vec::new();
    let mut total_cost = 0.0_f64;
    let mut have_cost = false;

    for (name, usage) in map {
        let detail = format!(
            "out {}, in {}, cache-read {}, cache-write {}",
            format_token_count(usage.output_tokens),
            format_token_count(usage.input_tokens),
            format_token_count(usage.cache_read_input_tokens),
            format_token_count(usage.cache_creation_input_tokens),
        );
        if any_cost {
            let cost_str = match usage.cost_usd {
                Some(cost) => {
                    total_cost += cost;
                    have_cost = true;
                    format!("${cost:.2}")
                }
                None => String::new(),
            };
            lines.push(format!("{name:<name_w$}{cost_str:<cost_w$}{detail}"));
        } else {
            lines.push(format!("{name:<name_w$}{detail}"));
        }
    }

    if have_cost {
        let total_label = "total";
        lines.push(format!("{total_label:<name_w$}${total_cost:.2}"));
    }

    lines
}

/// The JSON shape of a `tool` event's `detail`: `{"tool":"Bash","summary":
/// "...","agent":"..."}`. `summary` and `agent` are optional; older events
/// carry only `tool`. Kept private; callers only see the formatted or
/// counted results.
#[derive(serde::Deserialize)]
struct ToolDetail {
    tool: String,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    agent: Option<String>,
}

/// Renders a [`ModelUsageMap`] as one compact line, e.g. `fable-5 58.6k out
/// / sonnet-5 30.7k out`: one `{model} {out} out` segment per model, joined
/// by ` / `, with a leading `claude-` stripped from each model name for
/// brevity. Returns `None` for an empty map.
///
/// Shared by [`format_event_detail`]'s `"usage"` rendering and `tm ticket
/// audit`'s `Last audit usage:` line (see
/// `docs/plans/session-usage.md`'s "Surfaces" section).
pub fn format_model_usage_compact(map: &ModelUsageMap) -> Option<String> {
    if map.is_empty() {
        return None;
    }
    let parts: Vec<String> = map
        .iter()
        .map(|(name, usage)| {
            let short = name.strip_prefix("claude-").unwrap_or(name);
            format!("{short} {} out", format_token_count(usage.output_tokens))
        })
        .collect();
    Some(parts.join(" / "))
}

/// Renders a human-friendly one-line summary of a `run_events` row's
/// `detail`, for the known conventions a lane-run hook emits:
///
/// - `kind == "tool"`: the tool name, prefixed with `[<agent>]` when an
///   `agent` is present, suffixed with ` — <summary>` when a non-empty
///   `summary` is present. E.g. `{"tool":"Bash","summary":"cargo
///   test"}` -> `Bash — cargo test`.
/// - `kind == "checklist"`: `N/M done`, reusing the same detail shape as
///   [`latest_checklist`].
/// - `kind == "usage"`: one `{model} {out} out` segment per model, joined
///   by ` / `, e.g. `fable-5 89.2k out / sonnet-5 30.7k out`. A leading
///   `claude-` is stripped from each model name for brevity.
///
/// Returns `None` for any other kind, missing `detail`, or `detail` that
/// doesn't parse as the expected shape, so callers can fall back to
/// rendering the raw detail JSON.
pub fn format_event_detail(kind: &str, detail: Option<&str>) -> Option<String> {
    let detail = detail?;
    match kind {
        "tool" => {
            let parsed: ToolDetail = serde_json::from_str(detail).ok()?;
            let base = match parsed.agent.as_deref() {
                Some(agent) if !agent.is_empty() => format!("[{agent}] {}", parsed.tool),
                _ => parsed.tool,
            };
            Some(match parsed.summary.as_deref() {
                Some(summary) if !summary.is_empty() => format!("{base} — {summary}"),
                _ => base,
            })
        }
        "checklist" => {
            let parsed: ChecklistDetail = serde_json::from_str(detail).ok()?;
            let done = parsed.items.iter().filter(|item| item.done).count();
            Some(format!("{done}/{} done", parsed.items.len()))
        }
        "usage" => {
            let parsed: UsageDetail = serde_json::from_str(detail).ok()?;
            format_model_usage_compact(&parsed.models)
        }
        _ => None,
    }
}

/// Counts `tool` events in `events` by their `"tool"` name, skipping events
/// that aren't `kind == "tool"` or whose `detail` is missing or doesn't
/// parse as the [`ToolDetail`] shape.
///
/// Sorted by count descending, then tool name ascending, so the most-used
/// tools lead and ties are stable.
pub fn tool_counts(events: &[RunEvent]) -> Vec<(String, usize)> {
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for event in events {
        if event.kind != "tool" {
            continue;
        }
        let Some(detail) = event.detail.as_deref() else {
            continue;
        };
        let Ok(parsed) = serde_json::from_str::<ToolDetail>(detail) else {
            continue;
        };
        *counts.entry(parsed.tool).or_insert(0) += 1;
    }

    let mut counts: Vec<(String, usize)> = counts.into_iter().collect();
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    counts
}

/// Renders [`tool_counts`]'s output as a single summary line, e.g. `Tools:
/// Bash \u{d7}34, Edit \u{d7}8, Read \u{d7}10`. Returns `None` when `counts`
/// is empty so callers can omit the line entirely.
pub fn format_tool_counts(counts: &[(String, usize)]) -> Option<String> {
    if counts.is_empty() {
        return None;
    }
    let parts: Vec<String> = counts
        .iter()
        .map(|(name, n)| format!("{name} \u{d7}{n}"))
        .collect();
    Some(format!("Tools: {}", parts.join(", ")))
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
                "INSERT INTO runs (ticket, lane, status, worktree, branch, pid, kind, log_path, started_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, {NOW_SQL})"
            ),
            params![
                params.ticket,
                params.lane,
                RunStatus::Running.as_str(),
                params.worktree,
                params.branch,
                params.pid,
                params.kind,
                params.log_path,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Updates a run row's recorded `log_path` in place, without touching
    /// status or any other column.
    ///
    /// Exists for callers (`tm work run`'s detached path) that only know the
    /// log file's path *after* [`RunStore::start_run`] has already returned
    /// a row id — mirrors [`RunStore::update_pid`]'s shape for the same
    /// reason (the supervisor only learns its own pid after it starts).
    ///
    /// # Errors
    ///
    /// Returns [`RunStoreError::RunNotFound`] if `run_id` has no matching row.
    pub fn update_log_path(&self, run_id: i64, log_path: &str) -> Result<(), RunStoreError> {
        let changes = self.conn.execute(
            "UPDATE runs SET log_path = ?1 WHERE id = ?2",
            params![log_path, run_id],
        )?;

        if changes == 0 {
            return Err(RunStoreError::RunNotFound(run_id));
        }
        Ok(())
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
                    transcript = COALESCE(?8, transcript),
                    model_usage = COALESCE(?9, model_usage)
                 WHERE id = ?10"
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
                outcome.model_usage,
                run_id,
            ],
        )?;

        if changes == 0 {
            return Err(RunStoreError::RunNotFound(run_id));
        }
        Ok(())
    }

    /// Reopens a finished run, moving it back to `to` so it can be worked
    /// (or resumed) again.
    ///
    /// The precondition — the run's current status must be terminal (see
    /// [`RunStatus::is_terminal`]) — is enforced in the `UPDATE`'s `WHERE`
    /// clause itself, not just checked beforehand, so a concurrent writer
    /// can't race a non-terminal row past this guard. Clears `ended_at`,
    /// `pid`, and `heartbeat_at`: `ended_at` because a reopened run hasn't
    /// ended, and `pid`/`heartbeat_at` because the process that owned them is
    /// long gone — a stale pid or heartbeat from the *original* run would
    /// otherwise make the reopened row look like a live (or reapable) run
    /// that it isn't.
    ///
    /// Callers choosing `to`: [`RunStore::reap`] only ever touches
    /// `status = 'running'` rows, and reaps a `running` row with no pid on
    /// staleness alone (see `reap`'s doc comment) — since this method always
    /// clears `pid`, reopening straight to [`RunStatus::Running`] leaves the
    /// row one `tm runs reap` away from being marked failed again the moment
    /// its (inherited, already-old) `started_at` looks stale. Reopening to
    /// [`RunStatus::Queued`] avoids that trap; `tm runs reopen` defaults to
    /// it for exactly this reason.
    ///
    /// # Errors
    ///
    /// Returns [`RunStoreError::RunNotFound`] if `run_id` has no matching
    /// row, or [`RunStoreError::NotTerminal`] if it exists but its status
    /// isn't terminal.
    pub fn reopen_run(&self, run_id: i64, to: RunStatus) -> Result<ReopenedRun, RunStoreError> {
        let existing = self
            .run_by_id(run_id)?
            .ok_or(RunStoreError::RunNotFound(run_id))?;

        let changes = self.conn.execute(
            "UPDATE runs SET
                status = ?1,
                ended_at = NULL,
                pid = NULL,
                heartbeat_at = NULL
             WHERE id = ?2
               AND status IN ('done', 'failed', 'interrupted')",
            params![to.as_str(), run_id],
        )?;

        if changes == 0 {
            return Err(RunStoreError::NotTerminal {
                id: run_id,
                status: existing.status.as_str().to_string(),
            });
        }

        Ok(ReopenedRun {
            id: run_id,
            ticket: existing.ticket,
            old_status: existing.status,
            new_status: to,
        })
    }

    /// Updates a run row's recorded `pid` in place, without touching status,
    /// heartbeat, or any other column.
    ///
    /// Exists for the detached `tm work run` supervisor (see
    /// `docs/plans/runner-port.md` step 10): the row is created by the
    /// foreground `tm work run` invocation before it re-execs itself into a
    /// detached supervisor process, so the pid recorded at [`start_run`]
    /// time (if any) is the *parent's*, not the long-lived supervisor's.
    /// The supervisor calls this with its own `pid` immediately on startup
    /// so [`RunStore::reap`]'s liveness check (and any other pid-based
    /// tooling) probes the process that's actually still running the lane,
    /// not one that's already exited.
    ///
    /// [`start_run`]: RunStore::start_run
    ///
    /// # Errors
    ///
    /// Returns [`RunStoreError::RunNotFound`] if `run_id` has no matching row.
    pub fn update_pid(&self, run_id: i64, pid: u32) -> Result<(), RunStoreError> {
        let changes = self.conn.execute(
            "UPDATE runs SET pid = ?1 WHERE id = ?2",
            params![pid, run_id],
        )?;

        if changes == 0 {
            return Err(RunStoreError::RunNotFound(run_id));
        }
        Ok(())
    }

    /// Updates a run row's recorded `session_id` in place, without touching
    /// status, heartbeat, or any other column.
    ///
    /// Exists for [`crate::runs::session::register_session`]
    /// (`docs/plans/session-usage.md`): [`StartRun`] has no `session_id`
    /// field (it's normally only known at [`RunStore::finish_run`] time, via
    /// `claude -p`'s result), but a session run's id is known up front — it's
    /// the marker filename — so this stamps it onto the row [`start_run`]
    /// just created.
    ///
    /// [`start_run`]: RunStore::start_run
    ///
    /// # Errors
    ///
    /// Returns [`RunStoreError::RunNotFound`] if `run_id` has no matching row.
    pub fn update_session_id(&self, run_id: i64, session_id: &str) -> Result<(), RunStoreError> {
        let changes = self.conn.execute(
            "UPDATE runs SET session_id = ?1 WHERE id = ?2",
            params![session_id, run_id],
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
    ///
    /// Delegates to [`RunStore::list_runs_filtered`] with `kind: None`.
    pub fn list_runs(&self) -> Result<Vec<RunSummary>, RunStoreError> {
        self.list_runs_filtered(None)
    }

    /// Like [`RunStore::list_runs`], restricted to runs whose `kind` column
    /// equals `kind` when `Some`; `None` lists every kind (identical to
    /// [`RunStore::list_runs`]).
    pub fn list_runs_filtered(&self, kind: Option<&str>) -> Result<Vec<RunSummary>, RunStoreError> {
        let sql = "SELECT
                r.id,
                r.ticket,
                r.lane,
                r.kind,
                r.status,
                CAST((julianday('now') - julianday(r.started_at)) * 86400 AS INTEGER) AS age_secs,
                CASE WHEN r.ended_at IS NULL THEN
                    CAST((julianday('now') - julianday(COALESCE(r.heartbeat_at, r.started_at))) * 86400 AS INTEGER)
                ELSE NULL END AS heartbeat_age_secs,
                (SELECT e.kind FROM run_events e WHERE e.run_id = r.id ORDER BY e.at DESC, e.id DESC LIMIT 1) AS last_event_kind,
                (SELECT CAST((julianday('now') - julianday(e.at)) * 86400 AS INTEGER)
                    FROM run_events e WHERE e.run_id = r.id ORDER BY e.at DESC, e.id DESC LIMIT 1) AS last_event_age_secs
             FROM runs r
             WHERE ?1 IS NULL OR r.kind = ?1
             ORDER BY
                CASE r.status WHEN 'done' THEN 1 WHEN 'failed' THEN 1 WHEN 'interrupted' THEN 1 ELSE 0 END ASC,
                r.started_at DESC";

        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params![kind], |row| {
            let status_str: String = row.get(4)?;
            // Forward-compat fallback: a status string this binary doesn't
            // recognize (e.g. written by a newer binary sharing the same DB)
            // is ambiguous, not a known failure — Interrupted, not Failed.
            let status = RunStatus::parse(&status_str).unwrap_or(RunStatus::Interrupted);
            let last_event_kind: Option<String> = row.get(7)?;
            let awaiting_input = is_awaiting_input(status, last_event_kind.as_deref());
            Ok(RunSummary {
                id: row.get(0)?,
                ticket: row.get(1)?,
                lane: row.get(2)?,
                kind: row.get(3)?,
                status,
                age_secs: row.get(5)?,
                heartbeat_age_secs: row.get(6)?,
                last_event_kind,
                last_event_age_secs: row.get(8)?,
                awaiting_input,
            })
        })?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Marks abandoned runs as failed.
    ///
    /// A run is reaped when its status is `running`, its last heartbeat
    /// (falling back to `started_at`) is older than `stale_after_mins`, and
    /// its recorded pid is no longer alive (per `pid_alive`); rows with no
    /// recorded pid are reaped on staleness alone, since there's nothing to
    /// probe. Each reaped run gets `ended_at` set and a `reaped` event
    /// appended.
    ///
    /// Deliberately does not go through [`RunStore::add_event`]: that bumps
    /// `heartbeat_at`, which would be wrong to do for a run just declared
    /// dead.
    pub fn reap(
        &self,
        stale_after_mins: u64,
        pid_alive: &dyn Fn(u32) -> bool,
    ) -> Result<Vec<ReapedRun>, RunStoreError> {
        // stale_after_mins is a plain integer, not user-supplied text, so
        // it's safe to format directly into the modifier string rather than
        // trying to bind it inside strftime's modifier argument.
        let modifier = format!("-{stale_after_mins} minutes");

        let candidates: Vec<(i64, String, Option<u32>)> = {
            let mut stmt = self.conn.prepare(
                "SELECT id, ticket, pid FROM runs
                 WHERE status = 'running'
                   AND COALESCE(heartbeat_at, started_at) < strftime('%Y-%m-%dT%H:%M:%fZ','now',?1)",
            )?;
            let rows = stmt.query_map(params![modifier], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            out
        };

        let mut reaped = Vec::new();
        for (id, ticket, pid) in candidates {
            if let Some(p) = pid
                && pid_alive(p)
            {
                continue;
            }

            let tx = self.conn.unchecked_transaction()?;
            tx.execute(
                &format!("UPDATE runs SET status = 'failed', ended_at = {NOW_SQL} WHERE id = ?1"),
                params![id],
            )?;
            tx.execute(
                &format!(
                    "INSERT INTO run_events (run_id, at, kind, detail) VALUES (?1, {NOW_SQL}, 'reaped', NULL)"
                ),
                params![id],
            )?;
            tx.commit()?;

            reaped.push(ReapedRun { id, ticket, pid });
        }

        Ok(reaped)
    }

    /// Returns the latest run for `ticket` (by `started_at`, breaking ties
    /// by `id`, both descending), or `None` if it has no runs.
    ///
    /// Delegates to [`RunStore::latest_run_for_ticket_kind`] with
    /// `kind: None`.
    pub fn latest_run_for_ticket(&self, ticket: &str) -> Result<Option<Run>, RunStoreError> {
        self.latest_run_for_ticket_kind(ticket, None)
    }

    /// Like [`RunStore::latest_run_for_ticket`], restricted to runs whose
    /// `kind` column equals `kind` when `Some`; `None` matches every kind
    /// (identical to [`RunStore::latest_run_for_ticket`]).
    pub fn latest_run_for_ticket_kind(
        &self,
        ticket: &str,
        kind: Option<&str>,
    ) -> Result<Option<Run>, RunStoreError> {
        let sql = "SELECT
                id, ticket, lane, kind, status, session_id, worktree, branch, pid, transcript,
                started_at, heartbeat_at, ended_at, exit_code, num_turns, cost_usd,
                blocker, pr_url, model_usage, log_path,
                CAST((julianday('now') - julianday(started_at)) * 86400 AS INTEGER) AS age_secs
             FROM runs
             WHERE ticket = ?1 AND (?2 IS NULL OR kind = ?2)
             ORDER BY started_at DESC, id DESC
             LIMIT 1";

        self.conn
            .query_row(sql, params![ticket, kind], Self::row_to_run)
            .optional()
            .map_err(RunStoreError::from)
    }

    /// Returns the latest **finished** run for `ticket` with kind `kind`
    /// (status anything other than `queued`/`running`), by `started_at`
    /// breaking ties by `id`, both descending — or `None` if there is no
    /// such run.
    ///
    /// Used to find a ticket's most recent completed audit/create session
    /// once one is running concurrently (see `docs/plans/session-usage.md`'s
    /// "reap hazard" and "`tm runs show` resolves the latest run of any
    /// kind" ground truth): a still-running run of the same kind must not
    /// shadow the last *completed* one.
    pub fn latest_finished_run_for_ticket_kind(
        &self,
        ticket: &str,
        kind: &str,
    ) -> Result<Option<Run>, RunStoreError> {
        let sql = "SELECT
                id, ticket, lane, kind, status, session_id, worktree, branch, pid, transcript,
                started_at, heartbeat_at, ended_at, exit_code, num_turns, cost_usd,
                blocker, pr_url, model_usage, log_path,
                CAST((julianday('now') - julianday(started_at)) * 86400 AS INTEGER) AS age_secs
             FROM runs
             WHERE ticket = ?1 AND kind = ?2 AND status NOT IN ('running', 'queued')
             ORDER BY started_at DESC, id DESC
             LIMIT 1";

        self.conn
            .query_row(sql, params![ticket, kind], Self::row_to_run)
            .optional()
            .map_err(RunStoreError::from)
    }

    /// Returns the run with id `run_id`, or `None` if no such row exists.
    ///
    /// Used by `tm runs watch`'s detail window, which navigates by row id
    /// rather than ticket key (unlike [`RunStore::latest_run_for_ticket`],
    /// multiple runs can share a ticket key).
    pub fn run_by_id(&self, run_id: i64) -> Result<Option<Run>, RunStoreError> {
        let sql = "SELECT
                id, ticket, lane, kind, status, session_id, worktree, branch, pid, transcript,
                started_at, heartbeat_at, ended_at, exit_code, num_turns, cost_usd,
                blocker, pr_url, model_usage, log_path,
                CAST((julianday('now') - julianday(started_at)) * 86400 AS INTEGER) AS age_secs
             FROM runs
             WHERE id = ?1";

        self.conn
            .query_row(sql, params![run_id], Self::row_to_run)
            .optional()
            .map_err(RunStoreError::from)
    }

    /// Maps one row of the `id, ticket, lane, kind, status, session_id,
    /// worktree, branch, pid, transcript, started_at, heartbeat_at,
    /// ended_at, exit_code, num_turns, cost_usd, blocker, pr_url,
    /// model_usage, age_secs` projection (shared by [`RunStore::run_by_id`],
    /// [`RunStore::latest_run_for_ticket_kind`], and
    /// [`RunStore::latest_finished_run_for_ticket_kind`]) to a [`Run`].
    fn row_to_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<Run> {
        let status_str: String = row.get(4)?;
        // Same forward-compat reasoning as list_runs_filtered: an
        // unrecognized status string is ambiguous, not a known failure.
        let status = RunStatus::parse(&status_str).unwrap_or(RunStatus::Interrupted);
        Ok(Run {
            id: row.get(0)?,
            ticket: row.get(1)?,
            lane: row.get(2)?,
            kind: row.get(3)?,
            status,
            session_id: row.get(5)?,
            worktree: row.get(6)?,
            branch: row.get(7)?,
            pid: row.get(8)?,
            transcript: row.get(9)?,
            started_at: row.get(10)?,
            heartbeat_at: row.get(11)?,
            ended_at: row.get(12)?,
            exit_code: row.get(13)?,
            num_turns: row.get(14)?,
            cost_usd: row.get(15)?,
            blocker: row.get(16)?,
            pr_url: row.get(17)?,
            model_usage: row.get(18)?,
            log_path: row.get(19)?,
            age_secs: row.get(20)?,
        })
    }

    /// Returns all events for `run_id`, oldest first.
    pub fn events_for_run(&self, run_id: i64) -> Result<Vec<RunEvent>, RunStoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, at, kind, detail FROM run_events
             WHERE run_id = ?1
             ORDER BY at ASC, id ASC",
        )?;
        let rows = stmt.query_map(params![run_id], |row| {
            Ok(RunEvent {
                id: row.get(0)?,
                at: row.get(1)?,
                kind: row.get(2)?,
                detail: row.get(3)?,
            })
        })?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Records an audit verdict for `ticket_key`, with `audited_at` set to
    /// the database's current time (see [`NOW_SQL`]).
    ///
    /// Keeps full history rather than upserting: every call inserts a new
    /// row, so [`RunStore::latest_audit_for_ticket`] can report the most
    /// recent verdict while earlier ones remain queryable directly against
    /// the `ticket_audits` table.
    pub fn record_audit(
        &self,
        ticket_key: &str,
        verdict: &str,
        notes: Option<&str>,
    ) -> Result<(), RunStoreError> {
        self.conn.execute(
            &format!(
                "INSERT INTO ticket_audits (ticket_key, verdict, notes, audited_at)
                 VALUES (?1, ?2, ?3, {NOW_SQL})"
            ),
            params![ticket_key, verdict, notes],
        )?;
        Ok(())
    }

    /// Returns the most recently recorded audit for `ticket_key` (newest by
    /// `audited_at`, which is lexicographically sortable, breaking ties by
    /// `id`), or `None` if it has never been audited.
    pub fn latest_audit_for_ticket(
        &self,
        ticket_key: &str,
    ) -> Result<Option<TicketAudit>, RunStoreError> {
        self.conn
            .query_row(
                "SELECT ticket_key, verdict, notes, audited_at
                 FROM ticket_audits
                 WHERE ticket_key = ?1
                 ORDER BY audited_at DESC, id DESC
                 LIMIT 1",
                params![ticket_key],
                |row| {
                    Ok(TicketAudit {
                        ticket_key: row.get(0)?,
                        verdict: row.get(1)?,
                        notes: row.get(2)?,
                        audited_at: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(RunStoreError::from)
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
    fn run_status_interrupted_round_trips_through_as_str_and_parse() {
        assert_eq!(RunStatus::Interrupted.as_str(), "interrupted");
        assert_eq!(
            RunStatus::parse("interrupted"),
            Some(RunStatus::Interrupted)
        );
    }

    #[test]
    fn run_status_is_terminal_matches_done_failed_interrupted_only() {
        assert!(RunStatus::Done.is_terminal());
        assert!(RunStatus::Failed.is_terminal());
        assert!(RunStatus::Interrupted.is_terminal());
        assert!(!RunStatus::Queued.is_terminal());
        assert!(!RunStatus::Running.is_terminal());
        assert!(!RunStatus::Blocked.is_terminal());
        assert!(!RunStatus::Review.is_terminal());
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
            assert_eq!(version, MIGRATIONS.len() as i64);
        }

        let store = RunStore::open(&db_path).expect("reopen should succeed");
        let version: i64 = store
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
    }

    #[test]
    fn open_migrates_a_fresh_db_to_user_version_5() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let version: i64 = store
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 5);
    }

    #[test]
    fn log_path_column_defaults_to_null_for_rows_inserted_without_it() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "review-watch".to_string(),
                worktree: "/irrelevant".to_string(),
                branch: None,
                pid: None,
                kind: "review-watch".to_string(),
                log_path: None,
            })
            .unwrap();

        let run = store.run_by_id(id).unwrap().unwrap();
        assert_eq!(run.log_path, None);
    }

    #[test]
    fn start_run_round_trips_log_path() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "review-watch".to_string(),
                worktree: "/irrelevant".to_string(),
                branch: None,
                pid: None,
                kind: "review-watch".to_string(),
                log_path: Some(
                    "/home/user/.local/state/tskmstr/review-watch/proj-1.log".to_string(),
                ),
            })
            .unwrap();

        let run = store.run_by_id(id).unwrap().unwrap();
        assert_eq!(
            run.log_path.as_deref(),
            Some("/home/user/.local/state/tskmstr/review-watch/proj-1.log")
        );
    }

    #[test]
    fn update_log_path_overwrites_the_recorded_log_path_and_leaves_other_columns_alone() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "backend".to_string(),
                worktree: "/tmp/wt1".to_string(),
                branch: Some("owner/backend-20260101".to_string()),
                pid: None,
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();

        store
            .update_log_path(
                id,
                "/home/user/.local/state/tskmstr/work/backend-20260101.log",
            )
            .unwrap();

        let run = store.run_by_id(id).unwrap().unwrap();
        assert_eq!(
            run.log_path.as_deref(),
            Some("/home/user/.local/state/tskmstr/work/backend-20260101.log")
        );
        assert_eq!(run.branch.as_deref(), Some("owner/backend-20260101"));
    }

    #[test]
    fn update_log_path_unknown_id_returns_run_not_found() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let err = store
            .update_log_path(999, "/irrelevant.log")
            .expect_err("expected RunNotFound");

        match err {
            RunStoreError::RunNotFound(id) => assert_eq!(id, 999),
            other => panic!("expected RunNotFound, got {other:?}"),
        }
    }

    #[test]
    fn kind_column_defaults_to_lane_for_rows_inserted_without_it() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        // Bypass start_run to insert a row the way a pre-migration-4 writer
        // would have (no `kind` column in its INSERT), proving the `ALTER
        // TABLE ... DEFAULT 'lane'` migration backfills existing-shaped rows
        // rather than only ever being satisfied by start_run's explicit
        // value.
        store
            .conn
            .execute(
                &format!(
                    "INSERT INTO runs (ticket, lane, status, worktree, started_at)
                     VALUES ('PROJ-1', 'backend', 'running', '/tmp/wt1', {NOW_SQL})"
                ),
                [],
            )
            .unwrap();

        let kind: String = store
            .conn
            .query_row("SELECT kind FROM runs WHERE ticket = 'PROJ-1'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(kind, "lane");
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
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();
        let id2 = store
            .start_run(&StartRun {
                ticket: "PROJ-2".to_string(),
                lane: "backend".to_string(),
                worktree: "/tmp/wt2".to_string(),
                branch: None,
                pid: None,
                kind: "lane".to_string(),
                log_path: None,
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
    fn start_run_round_trips_kind_for_lane_and_audit_runs() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let lane_id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "backend".to_string(),
                worktree: "/tmp/wt1".to_string(),
                branch: None,
                pid: None,
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();
        let audit_id = store
            .start_run(&StartRun {
                ticket: "PROJ-2".to_string(),
                lane: "audit".to_string(),
                worktree: "/tmp/wt2".to_string(),
                branch: None,
                pid: None,
                kind: "audit".to_string(),
                log_path: None,
            })
            .unwrap();

        let lane_run = store.run_by_id(lane_id).unwrap().expect("expected a run");
        let audit_run = store.run_by_id(audit_id).unwrap().expect("expected a run");
        assert_eq!(lane_run.kind, "lane");
        assert_eq!(audit_run.kind, "audit");
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
                kind: "lane".to_string(),
                log_path: None,
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
                    model_usage: Some(r#"{"claude-fable-5":{"inputTokens":146}}"#.to_string()),
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
            Option<String>,
        );

        let (status, ended_at, exit_code, session_id, cost_usd, num_turns, pr_url, transcript, model_usage): FinishedRunRow = store
            .conn
            .query_row(
                "SELECT status, ended_at, exit_code, session_id, cost_usd, num_turns, pr_url, transcript, model_usage
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
                        row.get(8)?,
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
        assert_eq!(
            model_usage,
            Some(r#"{"claude-fable-5":{"inputTokens":146}}"#.to_string())
        );
    }

    #[test]
    fn finish_run_leaves_model_usage_untouched_when_none() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "backend".to_string(),
                worktree: "/tmp/wt1".to_string(),
                branch: None,
                pid: None,
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();

        store
            .finish_run(
                id,
                &FinishRun {
                    status: RunStatus::Done,
                    model_usage: Some(r#"{"claude-fable-5":{"inputTokens":1}}"#.to_string()),
                    ..FinishRun::default()
                },
            )
            .unwrap();
        store
            .finish_run(
                id,
                &FinishRun {
                    status: RunStatus::Done,
                    model_usage: None,
                    ..FinishRun::default()
                },
            )
            .unwrap();

        let run = store.run_by_id(id).unwrap().expect("expected a run");
        assert_eq!(
            run.model_usage,
            Some(r#"{"claude-fable-5":{"inputTokens":1}}"#.to_string())
        );
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

    /// Starts and finishes a run at `status`, returning its id.
    fn finished_run(store: &RunStore, status: RunStatus) -> i64 {
        let id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "backend".to_string(),
                worktree: "/tmp/wt1".to_string(),
                branch: None,
                pid: Some(4242),
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();
        store
            .finish_run(
                id,
                &FinishRun {
                    status,
                    ..FinishRun::default()
                },
            )
            .unwrap();
        id
    }

    #[test]
    fn reopen_run_succeeds_from_each_terminal_status() {
        for status in [RunStatus::Done, RunStatus::Failed, RunStatus::Interrupted] {
            let dir = tempdir().unwrap();
            let store = open_store(dir.path());
            let id = finished_run(&store, status);

            let reopened = store.reopen_run(id, RunStatus::Queued).unwrap();

            assert_eq!(reopened.id, id);
            assert_eq!(reopened.ticket, "PROJ-1");
            assert_eq!(reopened.old_status, status);
            assert_eq!(reopened.new_status, RunStatus::Queued);

            let run = store.run_by_id(id).unwrap().unwrap();
            assert_eq!(run.status, RunStatus::Queued);
            assert_eq!(run.ended_at, None);
            assert_eq!(run.pid, None);
            assert_eq!(run.heartbeat_at, None);
        }
    }

    #[test]
    fn reopen_run_can_target_running() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = finished_run(&store, RunStatus::Done);

        let reopened = store.reopen_run(id, RunStatus::Running).unwrap();

        assert_eq!(reopened.new_status, RunStatus::Running);
        let run = store.run_by_id(id).unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Running);
    }

    #[test]
    fn reopen_run_errors_on_non_terminal_status() {
        for status in [RunStatus::Queued, RunStatus::Running, RunStatus::Blocked] {
            let dir = tempdir().unwrap();
            let store = open_store(dir.path());
            let id = store
                .start_run(&StartRun {
                    ticket: "PROJ-1".to_string(),
                    lane: "backend".to_string(),
                    worktree: "/tmp/wt1".to_string(),
                    branch: None,
                    pid: None,
                    kind: "lane".to_string(),
                    log_path: None,
                })
                .unwrap();
            if status != RunStatus::Running {
                store
                    .finish_run(
                        id,
                        &FinishRun {
                            status,
                            ..FinishRun::default()
                        },
                    )
                    .unwrap();
            }

            let err = store
                .reopen_run(id, RunStatus::Queued)
                .expect_err("expected NotTerminal");

            match err {
                RunStoreError::NotTerminal {
                    id: err_id,
                    status: err_status,
                } => {
                    assert_eq!(err_id, id);
                    assert_eq!(err_status, status.as_str());
                }
                other => panic!("expected NotTerminal, got {other:?}"),
            }

            // Nothing changed.
            let run = store.run_by_id(id).unwrap().unwrap();
            assert_eq!(run.status, status);
        }
    }

    #[test]
    fn reopen_run_unknown_id_returns_run_not_found() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let err = store
            .reopen_run(999, RunStatus::Queued)
            .expect_err("expected RunNotFound");

        match err {
            RunStoreError::RunNotFound(id) => assert_eq!(id, 999),
            other => panic!("expected RunNotFound, got {other:?}"),
        }
    }

    #[test]
    fn update_pid_overwrites_the_recorded_pid_and_leaves_other_columns_alone() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "backend".to_string(),
                worktree: "/tmp/wt1".to_string(),
                branch: Some("owner/backend-20260101".to_string()),
                pid: None,
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();

        store.update_pid(id, 9999).unwrap();

        let run = store.run_by_id(id).unwrap().expect("expected a run");
        assert_eq!(run.pid, Some(9999));
        assert_eq!(run.status, RunStatus::Running);
        assert_eq!(run.branch, Some("owner/backend-20260101".to_string()));
    }

    #[test]
    fn update_pid_unknown_id_returns_run_not_found() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let err = store.update_pid(999, 1).expect_err("expected RunNotFound");

        match err {
            RunStoreError::RunNotFound(id) => assert_eq!(id, 999),
            other => panic!("expected RunNotFound, got {other:?}"),
        }
    }

    #[test]
    fn update_session_id_overwrites_the_recorded_session_id_and_leaves_other_columns_alone() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "audit".to_string(),
                worktree: "/tmp/wt1".to_string(),
                branch: None,
                pid: None,
                kind: "audit".to_string(),
                log_path: None,
            })
            .unwrap();

        store.update_session_id(id, "sess-abc").unwrap();

        let run = store.run_by_id(id).unwrap().expect("expected a run");
        assert_eq!(run.session_id, Some("sess-abc".to_string()));
        assert_eq!(run.ticket, "PROJ-1");
        assert_eq!(run.status, RunStatus::Running);
    }

    #[test]
    fn update_session_id_unknown_id_returns_run_not_found() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let err = store
            .update_session_id(999, "sess-abc")
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
                kind: "lane".to_string(),
                log_path: None,
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
                kind: "lane".to_string(),
                log_path: None,
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
                kind: "lane".to_string(),
                log_path: None,
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
                kind: "lane".to_string(),
                log_path: None,
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
    fn list_runs_surfaces_kind() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let lane_id = store
            .start_run(&StartRun {
                ticket: "PROJ-LANE".to_string(),
                lane: "backend".to_string(),
                worktree: "/tmp/wt-lane".to_string(),
                branch: None,
                pid: None,
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();
        let audit_id = store
            .start_run(&StartRun {
                ticket: "PROJ-AUDIT".to_string(),
                lane: "audit".to_string(),
                worktree: "/tmp/wt-audit".to_string(),
                branch: None,
                pid: None,
                kind: "audit".to_string(),
                log_path: None,
            })
            .unwrap();

        let runs = store.list_runs().unwrap();
        let lane_run = runs.iter().find(|r| r.id == lane_id).unwrap();
        let audit_run = runs.iter().find(|r| r.id == audit_id).unwrap();
        assert_eq!(lane_run.kind, "lane");
        assert_eq!(audit_run.kind, "audit");
    }

    #[test]
    fn list_runs_filtered_returns_only_matching_kind() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let lane_id = store
            .start_run(&StartRun {
                ticket: "PROJ-LANE".to_string(),
                lane: "backend".to_string(),
                worktree: "/tmp/wt-lane".to_string(),
                branch: None,
                pid: None,
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();
        let audit_id = store
            .start_run(&StartRun {
                ticket: "PROJ-AUDIT".to_string(),
                lane: "audit".to_string(),
                worktree: "/tmp/wt-audit".to_string(),
                branch: None,
                pid: None,
                kind: "audit".to_string(),
                log_path: None,
            })
            .unwrap();

        let audit_only = store.list_runs_filtered(Some("audit")).unwrap();
        assert_eq!(audit_only.len(), 1);
        assert_eq!(audit_only[0].id, audit_id);

        let all = store.list_runs_filtered(None).unwrap();
        let ids: Vec<i64> = all.iter().map(|r| r.id).collect();
        assert!(ids.contains(&lane_id));
        assert!(ids.contains(&audit_id));
    }

    #[test]
    fn is_awaiting_input_true_only_for_running_with_last_event_await() {
        assert!(is_awaiting_input(RunStatus::Running, Some("await")));
        assert!(!is_awaiting_input(RunStatus::Running, Some("resume")));
        assert!(!is_awaiting_input(RunStatus::Running, Some("tool")));
        assert!(!is_awaiting_input(RunStatus::Running, None));
        assert!(!is_awaiting_input(RunStatus::Done, Some("await")));
        assert!(!is_awaiting_input(RunStatus::Failed, Some("await")));
        assert!(!is_awaiting_input(RunStatus::Blocked, Some("await")));
        assert!(!is_awaiting_input(RunStatus::Queued, Some("await")));
        assert!(!is_awaiting_input(RunStatus::Review, Some("await")));
    }

    #[test]
    fn list_runs_filtered_marks_running_run_awaiting_after_await_event() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let id = store
            .start_run(&StartRun {
                ticket: "PROJ-AWAIT".to_string(),
                lane: "audit".to_string(),
                worktree: "/tmp/wt-await".to_string(),
                branch: None,
                pid: None,
                kind: "audit".to_string(),
                log_path: None,
            })
            .unwrap();
        store.add_event(id, "await", None).unwrap();

        let run = store
            .list_runs_filtered(None)
            .unwrap()
            .into_iter()
            .find(|r| r.id == id)
            .unwrap();
        assert_eq!(run.last_event_kind.as_deref(), Some("await"));
        assert!(run.awaiting_input, "await should flip awaiting_input on");
    }

    #[test]
    fn list_runs_filtered_clears_awaiting_after_resume_event() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let id = store
            .start_run(&StartRun {
                ticket: "PROJ-RESUME".to_string(),
                lane: "audit".to_string(),
                worktree: "/tmp/wt-resume".to_string(),
                branch: None,
                pid: None,
                kind: "audit".to_string(),
                log_path: None,
            })
            .unwrap();
        store.add_event(id, "await", None).unwrap();
        store.add_event(id, "resume", None).unwrap();

        let run = store
            .list_runs_filtered(None)
            .unwrap()
            .into_iter()
            .find(|r| r.id == id)
            .unwrap();
        assert_eq!(run.last_event_kind.as_deref(), Some("resume"));
        assert!(!run.awaiting_input, "resume should flip awaiting_input off");
    }

    #[test]
    fn list_runs_filtered_clears_awaiting_after_later_tool_event() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let id = store
            .start_run(&StartRun {
                ticket: "PROJ-TOOL".to_string(),
                lane: "audit".to_string(),
                worktree: "/tmp/wt-tool".to_string(),
                branch: None,
                pid: None,
                kind: "audit".to_string(),
                log_path: None,
            })
            .unwrap();
        store.add_event(id, "await", None).unwrap();
        store.add_event(id, "tool", None).unwrap();

        let run = store
            .list_runs_filtered(None)
            .unwrap()
            .into_iter()
            .find(|r| r.id == id)
            .unwrap();
        assert_eq!(run.last_event_kind.as_deref(), Some("tool"));
        assert!(
            !run.awaiting_input,
            "a later non-await event should flip awaiting_input off"
        );
    }

    #[test]
    fn list_runs_filtered_never_marks_a_finished_run_awaiting() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let id = store
            .start_run(&StartRun {
                ticket: "PROJ-DONE".to_string(),
                lane: "audit".to_string(),
                worktree: "/tmp/wt-done".to_string(),
                branch: None,
                pid: None,
                kind: "audit".to_string(),
                log_path: None,
            })
            .unwrap();
        store.add_event(id, "await", None).unwrap();
        store
            .finish_run(
                id,
                &FinishRun {
                    status: RunStatus::Done,
                    exit_code: None,
                    session_id: None,
                    cost_usd: None,
                    num_turns: None,
                    blocker: None,
                    pr_url: None,
                    transcript: None,
                    model_usage: None,
                },
            )
            .unwrap();

        let run = store
            .list_runs_filtered(None)
            .unwrap()
            .into_iter()
            .find(|r| r.id == id)
            .unwrap();
        assert_eq!(run.last_event_kind.as_deref(), Some("await"));
        assert!(
            !run.awaiting_input,
            "a finished run must never read as awaiting input, even with a trailing await event"
        );
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
                kind: "lane".to_string(),
                log_path: None,
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
                kind: "lane".to_string(),
                log_path: None,
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
                    kind: "lane".to_string(),
                    log_path: None,
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

    /// Backdates `run_id`'s heartbeat (falling back to `started_at` if there
    /// is none yet) to `minutes_ago` minutes in the past, so [`RunStore::reap`]
    /// sees it as stale.
    fn backdate_heartbeat(store: &RunStore, run_id: i64, minutes_ago: i64) {
        store
            .conn
            .execute(
                &format!(
                    "UPDATE runs SET heartbeat_at = strftime('%Y-%m-%dT%H:%M:%fZ','now','-{minutes_ago} minutes') WHERE id = ?1"
                ),
                params![run_id],
            )
            .unwrap();
    }

    fn always_alive(_pid: u32) -> bool {
        true
    }

    fn always_dead(_pid: u32) -> bool {
        false
    }

    #[test]
    fn reap_marks_stale_run_with_dead_pid_as_failed() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "backend".to_string(),
                worktree: "/tmp/wt1".to_string(),
                branch: None,
                pid: Some(4242),
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();
        backdate_heartbeat(&store, id, 20);

        let reaped = store.reap(10, &always_dead).unwrap();

        assert_eq!(reaped.len(), 1);
        assert_eq!(reaped[0].id, id);
        assert_eq!(reaped[0].ticket, "PROJ-1");
        assert_eq!(reaped[0].pid, Some(4242));

        let (status, ended_at): (String, Option<String>) = store
            .conn
            .query_row(
                "SELECT status, ended_at FROM runs WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "failed");
        assert!(ended_at.is_some());

        let event_kind: String = store
            .conn
            .query_row(
                "SELECT kind FROM run_events WHERE run_id = ?1",
                params![id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(event_kind, "reaped");
    }

    #[test]
    fn reap_leaves_stale_run_with_alive_pid_untouched() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "backend".to_string(),
                worktree: "/tmp/wt1".to_string(),
                branch: None,
                pid: Some(4242),
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();
        backdate_heartbeat(&store, id, 20);

        let reaped = store.reap(10, &always_alive).unwrap();

        assert!(reaped.is_empty());
        let status: String = store
            .conn
            .query_row(
                "SELECT status FROM runs WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "running");
    }

    #[test]
    fn reap_leaves_fresh_run_with_dead_pid_untouched() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "backend".to_string(),
                worktree: "/tmp/wt1".to_string(),
                branch: None,
                pid: Some(4242),
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();

        let reaped = store.reap(10, &always_dead).unwrap();

        assert!(reaped.is_empty());
        let status: String = store
            .conn
            .query_row(
                "SELECT status FROM runs WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "running");
    }

    #[test]
    fn reap_marks_stale_run_with_no_pid_as_failed() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "backend".to_string(),
                worktree: "/tmp/wt1".to_string(),
                branch: None,
                pid: None,
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();
        backdate_heartbeat(&store, id, 20);

        let reaped = store.reap(10, &always_alive).unwrap();

        assert_eq!(reaped.len(), 1);
        assert_eq!(reaped[0].pid, None);
    }

    #[test]
    fn reap_ignores_non_running_statuses_even_when_stale() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let blocked_id = store
            .start_run(&StartRun {
                ticket: "PROJ-BLOCKED".to_string(),
                lane: "backend".to_string(),
                worktree: "/tmp/wt1".to_string(),
                branch: None,
                pid: None,
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();
        store
            .finish_run(
                blocked_id,
                &FinishRun {
                    status: RunStatus::Blocked,
                    ..FinishRun::default()
                },
            )
            .unwrap();
        // finish_run sets ended_at; reset it so this looks like a
        // long-running-but-blocked row rather than a finished one.
        store
            .conn
            .execute(
                "UPDATE runs SET ended_at = NULL WHERE id = ?1",
                params![blocked_id],
            )
            .unwrap();
        backdate_heartbeat(&store, blocked_id, 20);

        let done_id = store
            .start_run(&StartRun {
                ticket: "PROJ-DONE".to_string(),
                lane: "backend".to_string(),
                worktree: "/tmp/wt2".to_string(),
                branch: None,
                pid: None,
                kind: "lane".to_string(),
                log_path: None,
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
        backdate_heartbeat(&store, done_id, 20);

        let reaped = store.reap(10, &always_dead).unwrap();

        assert!(reaped.is_empty());
    }

    #[test]
    fn latest_run_for_ticket_returns_none_when_no_runs() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        assert!(store.latest_run_for_ticket("PROJ-1").unwrap().is_none());
    }

    #[test]
    fn latest_run_for_ticket_picks_most_recent_started_at() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let older_id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "backend".to_string(),
                worktree: "/tmp/wt-older".to_string(),
                branch: None,
                pid: None,
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE runs SET started_at = '2020-01-01T00:00:00.000Z' WHERE id = ?1",
                params![older_id],
            )
            .unwrap();

        let newer_id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "backend".to_string(),
                worktree: "/tmp/wt-newer".to_string(),
                branch: None,
                pid: None,
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE runs SET started_at = '2020-06-01T00:00:00.000Z' WHERE id = ?1",
                params![newer_id],
            )
            .unwrap();

        let run = store
            .latest_run_for_ticket("PROJ-1")
            .unwrap()
            .expect("expected a run");
        assert_eq!(run.id, newer_id);
        assert_eq!(run.ticket, "PROJ-1");
        assert_eq!(run.worktree, "/tmp/wt-newer");
        assert!(run.age_secs >= 0);
    }

    #[test]
    fn latest_run_for_ticket_breaks_started_at_tie_by_id() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let first_id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "backend".to_string(),
                worktree: "/tmp/wt-first".to_string(),
                branch: None,
                pid: None,
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();
        let second_id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "backend".to_string(),
                worktree: "/tmp/wt-second".to_string(),
                branch: None,
                pid: None,
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();

        let same_started_at = "2020-06-01T00:00:00.000Z";
        store
            .conn
            .execute(
                "UPDATE runs SET started_at = ?1 WHERE id IN (?2, ?3)",
                params![same_started_at, first_id, second_id],
            )
            .unwrap();

        let run = store
            .latest_run_for_ticket("PROJ-1")
            .unwrap()
            .expect("expected a run");
        assert_eq!(run.id, second_id);
    }

    #[test]
    fn latest_run_for_ticket_kind_none_matches_latest_run_for_ticket() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "backend".to_string(),
                worktree: "/tmp/wt-lane".to_string(),
                branch: None,
                pid: None,
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();
        let audit_id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "audit".to_string(),
                worktree: "/tmp/wt-audit".to_string(),
                branch: None,
                pid: None,
                kind: "audit".to_string(),
                log_path: None,
            })
            .unwrap();

        let via_kind_none = store
            .latest_run_for_ticket_kind("PROJ-1", None)
            .unwrap()
            .expect("expected a run");
        let via_plain = store
            .latest_run_for_ticket("PROJ-1")
            .unwrap()
            .expect("expected a run");
        assert_eq!(via_kind_none.id, audit_id);
        assert_eq!(via_kind_none.id, via_plain.id);
    }

    #[test]
    fn latest_run_for_ticket_kind_filters_to_the_given_kind() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let lane_id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "backend".to_string(),
                worktree: "/tmp/wt-lane".to_string(),
                branch: None,
                pid: None,
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();
        // Started after the lane run, but a different kind, so it must not
        // shadow the lane-kind lookup below.
        store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "audit".to_string(),
                worktree: "/tmp/wt-audit".to_string(),
                branch: None,
                pid: None,
                kind: "audit".to_string(),
                log_path: None,
            })
            .unwrap();

        let run = store
            .latest_run_for_ticket_kind("PROJ-1", Some("lane"))
            .unwrap()
            .expect("expected a run");
        assert_eq!(run.id, lane_id);
        assert_eq!(run.kind, "lane");
    }

    #[test]
    fn review_watch_and_bugbot_cleanup_kinds_round_trip_with_no_migration() {
        // `kind` is unconstrained free text (`TEXT NOT NULL DEFAULT 'lane'`,
        // no `CHECK`), so the two new kinds bugbot-watch introduces
        // (`review-watch` for the watcher, `bugbot-cleanup` for the triage
        // session) need no migration — this confirms it end to end through
        // start_run, list_runs, and latest_run_for_ticket_kind, mirroring
        // latest_run_for_ticket_kind_filters_to_the_given_kind above.
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let watch_id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "review-watch".to_string(),
                worktree: "/tmp/wt-watch".to_string(),
                branch: None,
                pid: None,
                kind: "review-watch".to_string(),
                log_path: None,
            })
            .unwrap();
        let cleanup_id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "bugbot-cleanup".to_string(),
                worktree: "/tmp/wt-cleanup".to_string(),
                branch: None,
                pid: None,
                kind: "bugbot-cleanup".to_string(),
                log_path: None,
            })
            .unwrap();

        let watch_run = store.run_by_id(watch_id).unwrap().expect("expected a run");
        let cleanup_run = store
            .run_by_id(cleanup_id)
            .unwrap()
            .expect("expected a run");
        assert_eq!(watch_run.kind, "review-watch");
        assert_eq!(cleanup_run.kind, "bugbot-cleanup");

        let kinds_in_list: Vec<String> = store
            .list_runs()
            .unwrap()
            .into_iter()
            .map(|r| r.kind)
            .collect();
        assert!(kinds_in_list.contains(&"review-watch".to_string()));
        assert!(kinds_in_list.contains(&"bugbot-cleanup".to_string()));

        let latest_watch = store
            .latest_run_for_ticket_kind("PROJ-1", Some("review-watch"))
            .unwrap()
            .expect("expected a run");
        assert_eq!(latest_watch.id, watch_id);

        let latest_cleanup = store
            .latest_run_for_ticket_kind("PROJ-1", Some("bugbot-cleanup"))
            .unwrap()
            .expect("expected a run");
        assert_eq!(latest_cleanup.id, cleanup_id);
    }

    #[test]
    fn latest_finished_run_for_ticket_kind_ignores_running_and_other_kinds() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        // A still-running audit run for the ticket: must be ignored in
        // favor of the finished one below, per the "reap hazard" / "show
        // shadowing" ground truth in docs/plans/session-usage.md.
        store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "audit".to_string(),
                worktree: "/tmp/wt-running".to_string(),
                branch: None,
                pid: None,
                kind: "audit".to_string(),
                log_path: None,
            })
            .unwrap();

        // A finished run of a different kind: must also be ignored.
        let create_id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "create".to_string(),
                worktree: "/tmp/wt-create".to_string(),
                branch: None,
                pid: None,
                kind: "create".to_string(),
                log_path: None,
            })
            .unwrap();
        store
            .finish_run(
                create_id,
                &FinishRun {
                    status: RunStatus::Done,
                    ..FinishRun::default()
                },
            )
            .unwrap();

        // An older finished audit run.
        let older_audit_id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "audit".to_string(),
                worktree: "/tmp/wt-older".to_string(),
                branch: None,
                pid: None,
                kind: "audit".to_string(),
                log_path: None,
            })
            .unwrap();
        store
            .finish_run(
                older_audit_id,
                &FinishRun {
                    status: RunStatus::Done,
                    ..FinishRun::default()
                },
            )
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE runs SET started_at = '2020-01-01T00:00:00.000Z' WHERE id = ?1",
                params![older_audit_id],
            )
            .unwrap();

        // The latest finished audit run.
        let newer_audit_id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "audit".to_string(),
                worktree: "/tmp/wt-newer".to_string(),
                branch: None,
                pid: None,
                kind: "audit".to_string(),
                log_path: None,
            })
            .unwrap();
        store
            .finish_run(
                newer_audit_id,
                &FinishRun {
                    status: RunStatus::Done,
                    ..FinishRun::default()
                },
            )
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE runs SET started_at = '2020-06-01T00:00:00.000Z' WHERE id = ?1",
                params![newer_audit_id],
            )
            .unwrap();

        let run = store
            .latest_finished_run_for_ticket_kind("PROJ-1", "audit")
            .unwrap()
            .expect("expected a finished audit run");
        assert_eq!(run.id, newer_audit_id);
        assert_eq!(run.kind, "audit");
        assert_eq!(run.status, RunStatus::Done);
    }

    #[test]
    fn latest_finished_run_for_ticket_kind_returns_none_when_none_finished() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "audit".to_string(),
                worktree: "/tmp/wt-running".to_string(),
                branch: None,
                pid: None,
                kind: "audit".to_string(),
                log_path: None,
            })
            .unwrap();

        assert!(
            store
                .latest_finished_run_for_ticket_kind("PROJ-1", "audit")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn run_by_id_returns_none_when_no_such_row() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        assert!(store.run_by_id(999).unwrap().is_none());
    }

    #[test]
    fn run_by_id_returns_the_matching_row() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        let other_id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "backend".to_string(),
                worktree: "/tmp/wt-other".to_string(),
                branch: None,
                pid: None,
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();
        let id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "backend".to_string(),
                worktree: "/tmp/wt-target".to_string(),
                branch: Some("proj-1".to_string()),
                pid: Some(4242),
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();
        assert_ne!(other_id, id);

        let run = store.run_by_id(id).unwrap().expect("expected a run");
        assert_eq!(run.id, id);
        assert_eq!(run.worktree, "/tmp/wt-target");
        assert_eq!(run.branch, Some("proj-1".to_string()));
        assert_eq!(run.pid, Some(4242));
        assert!(run.age_secs >= 0);
    }

    #[test]
    fn events_for_run_returns_empty_vec_when_none() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let run_id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "backend".to_string(),
                worktree: "/tmp/wt1".to_string(),
                branch: None,
                pid: None,
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();

        let events = store.events_for_run(run_id).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn events_for_run_orders_oldest_first() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let run_id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                lane: "backend".to_string(),
                worktree: "/tmp/wt1".to_string(),
                branch: None,
                pid: None,
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();

        let second_id = store.add_event(run_id, "second", None).unwrap();
        store
            .conn
            .execute(
                "UPDATE run_events SET at = '2020-06-01T00:00:00.000Z' WHERE id = ?1",
                params![second_id],
            )
            .unwrap();
        let first_id = store.add_event(run_id, "first", Some("detail")).unwrap();
        store
            .conn
            .execute(
                "UPDATE run_events SET at = '2020-01-01T00:00:00.000Z' WHERE id = ?1",
                params![first_id],
            )
            .unwrap();

        let events = store.events_for_run(run_id).unwrap();
        let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, vec!["first", "second"]);
        assert_eq!(events[0].detail.as_deref(), Some("detail"));
    }

    #[test]
    fn latest_audit_for_ticket_returns_none_when_never_audited() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        assert!(store.latest_audit_for_ticket("PROJ-1").unwrap().is_none());
    }

    #[test]
    fn record_audit_round_trips_verdict_and_notes() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        store
            .record_audit("PROJ-1", "ready", Some("looks good"))
            .unwrap();

        let audit = store
            .latest_audit_for_ticket("PROJ-1")
            .unwrap()
            .expect("expected an audit");
        assert_eq!(audit.ticket_key, "PROJ-1");
        assert_eq!(audit.verdict, "ready");
        assert_eq!(audit.notes.as_deref(), Some("looks good"));
        let re = Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$").unwrap();
        assert!(
            re.is_match(&audit.audited_at),
            "audited_at {:?} did not match expected format",
            audit.audited_at
        );
    }

    #[test]
    fn record_audit_notes_are_optional() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        store.record_audit("PROJ-1", "needs-work", None).unwrap();

        let audit = store
            .latest_audit_for_ticket("PROJ-1")
            .unwrap()
            .expect("expected an audit");
        assert_eq!(audit.verdict, "needs-work");
        assert_eq!(audit.notes, None);
    }

    #[test]
    fn latest_audit_for_ticket_prefers_most_recently_recorded() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        store.record_audit("PROJ-1", "needs-work", None).unwrap();
        store
            .conn
            .execute(
                "UPDATE ticket_audits SET audited_at = '2000-01-01T00:00:00.000Z' WHERE ticket_key = 'PROJ-1'",
                [],
            )
            .unwrap();
        store
            .record_audit("PROJ-1", "ready", Some("second pass"))
            .unwrap();

        let audit = store
            .latest_audit_for_ticket("PROJ-1")
            .unwrap()
            .expect("expected an audit");
        assert_eq!(audit.verdict, "ready");
        assert_eq!(audit.notes.as_deref(), Some("second pass"));
    }

    fn checklist_event(id: i64, detail: Option<&str>) -> RunEvent {
        RunEvent {
            id,
            at: format!("2020-01-01T00:00:{id:02}.000Z"),
            kind: "checklist".to_string(),
            detail: detail.map(str::to_string),
        }
    }

    fn make_event(id: i64, kind: &str, detail: Option<&str>) -> RunEvent {
        RunEvent {
            id,
            at: format!("2020-01-01T00:00:{id:02}.000Z"),
            kind: kind.to_string(),
            detail: detail.map(str::to_string),
        }
    }

    #[test]
    fn format_event_detail_renders_tool_with_summary() {
        let rendered =
            format_event_detail("tool", Some(r#"{"tool":"Bash","summary":"cargo test"}"#));
        assert_eq!(rendered, Some("Bash — cargo test".to_string()));
    }

    #[test]
    fn format_event_detail_renders_tool_with_agent_and_summary() {
        let rendered = format_event_detail(
            "tool",
            Some(r#"{"tool":"Read","summary":"src/main.rs","agent":"Explore"}"#),
        );
        assert_eq!(rendered, Some("[Explore] Read — src/main.rs".to_string()));
    }

    #[test]
    fn format_event_detail_renders_tool_name_only() {
        let rendered = format_event_detail("tool", Some(r#"{"tool":"Bash"}"#));
        assert_eq!(rendered, Some("Bash".to_string()));
    }

    #[test]
    fn format_event_detail_ignores_empty_summary_and_agent() {
        let rendered =
            format_event_detail("tool", Some(r#"{"tool":"Bash","summary":"","agent":""}"#));
        assert_eq!(rendered, Some("Bash".to_string()));
    }

    #[test]
    fn format_event_detail_renders_checklist_progress() {
        let rendered = format_event_detail(
            "checklist",
            Some(r#"{"items":[{"text":"a","done":true},{"text":"b","done":false}]}"#),
        );
        assert_eq!(rendered, Some("1/2 done".to_string()));
    }

    #[test]
    fn format_event_detail_returns_none_for_unknown_kind() {
        let rendered = format_event_detail("stop", Some(r#"{"tool":"Bash"}"#));
        assert_eq!(rendered, None);
    }

    #[test]
    fn format_event_detail_returns_none_for_malformed_tool_detail() {
        let rendered = format_event_detail("tool", Some("not json"));
        assert_eq!(rendered, None);
    }

    #[test]
    fn format_event_detail_returns_none_for_missing_detail() {
        assert_eq!(format_event_detail("tool", None), None);
        assert_eq!(format_event_detail("checklist", None), None);
    }

    #[test]
    fn tool_counts_counts_and_sorts_by_count_desc_then_name_asc() {
        let events = vec![
            make_event(1, "tool", Some(r#"{"tool":"Read"}"#)),
            make_event(2, "tool", Some(r#"{"tool":"Bash"}"#)),
            make_event(3, "tool", Some(r#"{"tool":"Bash"}"#)),
            make_event(4, "tool", Some(r#"{"tool":"Edit"}"#)),
            make_event(5, "tool", Some(r#"{"tool":"Bash"}"#)),
        ];

        let counts = tool_counts(&events);

        assert_eq!(
            counts,
            vec![
                ("Bash".to_string(), 3),
                ("Edit".to_string(), 1),
                ("Read".to_string(), 1),
            ]
        );
    }

    #[test]
    fn tool_counts_skips_non_tool_events_and_malformed_detail() {
        let events = vec![
            make_event(1, "tool", Some(r#"{"tool":"Bash"}"#)),
            make_event(2, "checklist", Some(r#"{"items":[]}"#)),
            make_event(3, "tool", Some("not json")),
            make_event(4, "tool", None),
        ];

        assert_eq!(tool_counts(&events), vec![("Bash".to_string(), 1)]);
    }

    #[test]
    fn tool_counts_returns_empty_vec_when_no_tool_events() {
        let events = vec![make_event(1, "stop", None)];

        assert_eq!(tool_counts(&events), Vec::<(String, usize)>::new());
    }

    #[test]
    fn format_tool_counts_renders_multiplication_sign_line() {
        let counts = vec![
            ("Bash".to_string(), 34),
            ("Edit".to_string(), 8),
            ("Read".to_string(), 10),
        ];

        assert_eq!(
            format_tool_counts(&counts),
            Some("Tools: Bash \u{d7}34, Edit \u{d7}8, Read \u{d7}10".to_string())
        );
    }

    #[test]
    fn format_tool_counts_returns_none_for_empty() {
        assert_eq!(format_tool_counts(&[]), None);
    }

    #[test]
    fn latest_checklist_parses_happy_path() {
        let events = vec![checklist_event(
            1,
            Some(
                r#"{"items":[{"text":"write tests","done":true},{"text":"implement","done":false}]}"#,
            ),
        )];

        let state = latest_checklist(&events).expect("expected a checklist");
        assert_eq!(state.items.len(), 2);
        assert_eq!(state.items[0].text, "write tests");
        assert!(state.items[0].done);
        assert_eq!(state.items[1].text, "implement");
        assert!(!state.items[1].done);
        assert_eq!(state.done_count(), 1);
    }

    #[test]
    fn latest_checklist_prefers_the_newest_event() {
        let events = vec![
            checklist_event(1, Some(r#"{"items":[{"text":"old","done":false}]}"#)),
            checklist_event(2, Some(r#"{"items":[{"text":"new","done":true}]}"#)),
        ];

        let state = latest_checklist(&events).expect("expected a checklist");
        assert_eq!(state.items.len(), 1);
        assert_eq!(state.items[0].text, "new");
        assert!(state.items[0].done);
    }

    #[test]
    fn latest_checklist_falls_back_when_the_newest_detail_is_malformed() {
        let events = vec![
            checklist_event(1, Some(r#"{"items":[{"text":"good","done":true}]}"#)),
            checklist_event(2, Some("not json at all")),
        ];

        let state = latest_checklist(&events).expect("expected a fallback checklist");
        assert_eq!(state.items[0].text, "good");
    }

    #[test]
    fn latest_checklist_falls_back_when_the_newest_detail_is_missing() {
        let events = vec![
            checklist_event(1, Some(r#"{"items":[{"text":"good","done":false}]}"#)),
            checklist_event(2, None),
        ];

        let state = latest_checklist(&events).expect("expected a fallback checklist");
        assert_eq!(state.items[0].text, "good");
    }

    #[test]
    fn latest_checklist_returns_none_when_no_checklist_events() {
        let events = vec![RunEvent {
            id: 1,
            at: "2020-01-01T00:00:01.000Z".to_string(),
            kind: "tool_use".to_string(),
            detail: None,
        }];

        assert!(latest_checklist(&events).is_none());
    }

    #[test]
    fn latest_checklist_returns_none_when_every_checklist_event_is_malformed() {
        let events = vec![
            checklist_event(1, Some("garbage")),
            checklist_event(2, None),
        ];

        assert!(latest_checklist(&events).is_none());
    }

    #[test]
    fn latest_checklist_ignores_unknown_extra_json_fields() {
        let events = vec![checklist_event(
            1,
            Some(r#"{"items":[{"text":"a","done":true,"note":"extra"}],"schema_version":2}"#),
        )];

        let state = latest_checklist(&events).expect("expected a checklist");
        assert_eq!(state.items.len(), 1);
        assert_eq!(state.items[0].text, "a");
    }

    #[test]
    fn latest_checklist_handles_empty_items() {
        let events = vec![checklist_event(1, Some(r#"{"items":[]}"#))];

        let state = latest_checklist(&events).expect("expected a checklist");
        assert!(state.items.is_empty());
        assert_eq!(state.done_count(), 0);
    }

    fn usage_event(id: i64, detail: Option<&str>) -> RunEvent {
        RunEvent {
            id,
            at: format!("2020-01-01T00:00:{id:02}.000Z"),
            kind: "usage".to_string(),
            detail: detail.map(str::to_string),
        }
    }

    #[test]
    fn latest_usage_parses_happy_path() {
        let events = vec![usage_event(
            1,
            Some(
                r#"{"models":{"claude-fable-5":{"inputTokens":146,"outputTokens":58564,"cacheReadInputTokens":6535803,"cacheCreationInputTokens":203983}}}"#,
            ),
        )];

        let usage = latest_usage(&events).expect("expected usage");
        let fable = usage.get("claude-fable-5").expect("expected fable-5 entry");
        assert_eq!(fable.input_tokens, 146);
        assert_eq!(fable.output_tokens, 58564);
        assert_eq!(fable.cache_read_input_tokens, 6535803);
        assert_eq!(fable.cache_creation_input_tokens, 203983);
        assert_eq!(fable.cost_usd, None);
    }

    #[test]
    fn latest_usage_prefers_the_newest_event() {
        let events = vec![
            usage_event(
                1,
                Some(r#"{"models":{"claude-fable-5":{"outputTokens":1}}}"#),
            ),
            usage_event(
                2,
                Some(r#"{"models":{"claude-fable-5":{"outputTokens":2}}}"#),
            ),
        ];

        let usage = latest_usage(&events).expect("expected usage");
        assert_eq!(usage.get("claude-fable-5").unwrap().output_tokens, 2);
    }

    #[test]
    fn latest_usage_falls_back_when_the_newest_detail_is_malformed() {
        let events = vec![
            usage_event(
                1,
                Some(r#"{"models":{"claude-fable-5":{"outputTokens":1}}}"#),
            ),
            usage_event(2, Some("not json")),
        ];

        let usage = latest_usage(&events).expect("expected a fallback usage");
        assert_eq!(usage.get("claude-fable-5").unwrap().output_tokens, 1);
    }

    #[test]
    fn latest_usage_returns_none_when_no_usage_events() {
        let events = vec![make_event(1, "tool_use", None)];
        assert!(latest_usage(&events).is_none());
    }

    #[test]
    fn latest_usage_returns_none_when_every_usage_event_is_malformed() {
        let events = vec![usage_event(1, Some("garbage")), usage_event(2, None)];
        assert!(latest_usage(&events).is_none());
    }

    #[test]
    fn parse_model_usage_parses_bare_map() {
        let usage =
            parse_model_usage(r#"{"claude-fable-5":{"outputTokens":58564,"costUSD":12.996}}"#)
                .expect("expected a map");
        let fable = usage.get("claude-fable-5").unwrap();
        assert_eq!(fable.output_tokens, 58564);
        assert_eq!(fable.cost_usd, Some(12.996));
    }

    #[test]
    fn parse_model_usage_returns_none_for_malformed_json() {
        assert!(parse_model_usage("not json").is_none());
    }

    fn agent_usage_event(id: i64, detail: Option<&str>) -> RunEvent {
        RunEvent {
            id,
            at: format!("2020-01-01T00:00:{id:02}.000Z"),
            kind: "agent_usage".to_string(),
            detail: detail.map(str::to_string),
        }
    }

    /// The plan's example `agent_usage` detail JSON
    /// (`docs/plans/per-agent-usage.md`, "Schema/event design").
    const AGENT_USAGE_FIXTURE: &str = r#"{"agentType": "elixir-implementer", "description": "Implement AX-404 UI threading", "model": "claude-sonnet-5", "outputTokens": 1143, "inputTokens": 2, "cacheReadInputTokens": 87519, "cacheCreationInputTokens": 3012, "totalToolUseCount": 38, "durationMs": 193659}"#;

    #[test]
    fn collect_agent_usage_parses_happy_path() {
        let events = vec![agent_usage_event(1, Some(AGENT_USAGE_FIXTURE))];

        let collected = collect_agent_usage(&events);

        assert_eq!(collected.len(), 1);
        let event = &collected[0];
        assert_eq!(event.agent_type, "elixir-implementer");
        assert_eq!(
            event.description.as_deref(),
            Some("Implement AX-404 UI threading")
        );
        assert_eq!(event.model, "claude-sonnet-5");
        assert_eq!(event.usage.output_tokens, 1143);
        assert_eq!(event.usage.input_tokens, 2);
        assert_eq!(event.usage.cache_read_input_tokens, 87519);
        assert_eq!(event.usage.cache_creation_input_tokens, 3012);
        assert_eq!(event.usage.cost_usd, None);
        assert_eq!(event.total_tool_use_count, 38);
        assert_eq!(event.duration_ms, 193659);
    }

    #[test]
    fn collect_agent_usage_skips_malformed_detail() {
        let events = vec![
            agent_usage_event(1, Some(AGENT_USAGE_FIXTURE)),
            agent_usage_event(2, Some("not json")),
            agent_usage_event(3, None),
            agent_usage_event(4, Some(r#"{"description":"missing required fields"}"#)),
        ];

        let collected = collect_agent_usage(&events);

        assert_eq!(collected.len(), 1);
        assert_eq!(collected[0].agent_type, "elixir-implementer");
    }

    #[test]
    fn collect_agent_usage_preserves_order_oldest_first() {
        let events = vec![
            agent_usage_event(
                1,
                Some(r#"{"agentType":"Explore","model":"claude-fable-5","outputTokens":1}"#),
            ),
            agent_usage_event(
                2,
                Some(
                    r#"{"agentType":"elixir-implementer","model":"claude-sonnet-5","outputTokens":2}"#,
                ),
            ),
        ];

        let collected = collect_agent_usage(&events);

        assert_eq!(collected.len(), 2);
        assert_eq!(collected[0].agent_type, "Explore");
        assert_eq!(collected[1].agent_type, "elixir-implementer");
    }

    #[test]
    fn collect_agent_usage_returns_empty_when_no_agent_usage_events() {
        let events = vec![make_event(1, "tool", None), checklist_event(2, None)];

        assert_eq!(collect_agent_usage(&events), Vec::new());
    }

    #[test]
    fn collect_agent_usage_picks_real_event_out_of_mixed_kinds() {
        let events = vec![
            make_event(1, "tool", Some(r#"{"tool":"Bash","summary":"cargo test"}"#)),
            usage_event(
                2,
                Some(r#"{"models":{"claude-fable-5":{"outputTokens":1}}}"#),
            ),
            checklist_event(3, Some(r#"{"items":[{"text":"a","done":true}]}"#)),
            agent_usage_event(4, Some(AGENT_USAGE_FIXTURE)),
        ];

        let collected = collect_agent_usage(&events);

        assert_eq!(collected.len(), 1);
        assert_eq!(collected[0].agent_type, "elixir-implementer");
        assert_eq!(collected[0].model, "claude-sonnet-5");
    }

    #[test]
    fn aggregate_agent_usage_sums_a_single_invocation() {
        let events = collect_agent_usage(&[agent_usage_event(1, Some(AGENT_USAGE_FIXTURE))]);

        let totals = aggregate_agent_usage(&events);

        let key = (
            "elixir-implementer".to_string(),
            "claude-sonnet-5".to_string(),
        );
        let entry = totals.get(&key).expect("expected an entry");
        assert_eq!(totals.len(), 1);
        assert_eq!(entry.invocations, 1);
        assert_eq!(entry.output_tokens, 1143);
        assert_eq!(entry.input_tokens, 2);
        assert_eq!(entry.cache_read_input_tokens, 87519);
        assert_eq!(entry.cache_creation_input_tokens, 3012);
        assert_eq!(entry.total_tool_use_count, 38);
        assert_eq!(entry.duration_ms, 193659);
    }

    #[test]
    fn aggregate_agent_usage_sums_repeated_same_type_invocations() {
        let events = collect_agent_usage(&[
            agent_usage_event(
                1,
                Some(
                    r#"{"agentType":"elixir-implementer","model":"claude-sonnet-5","outputTokens":100,"totalToolUseCount":5,"durationMs":1000}"#,
                ),
            ),
            agent_usage_event(
                2,
                Some(
                    r#"{"agentType":"elixir-implementer","model":"claude-sonnet-5","outputTokens":200,"totalToolUseCount":7,"durationMs":2000}"#,
                ),
            ),
        ]);

        let totals = aggregate_agent_usage(&events);

        let key = (
            "elixir-implementer".to_string(),
            "claude-sonnet-5".to_string(),
        );
        let entry = totals.get(&key).expect("expected an entry");
        assert_eq!(totals.len(), 1);
        assert_eq!(entry.invocations, 2);
        assert_eq!(entry.output_tokens, 300);
        assert_eq!(entry.total_tool_use_count, 12);
        assert_eq!(entry.duration_ms, 3000);
    }

    #[test]
    fn aggregate_agent_usage_keeps_same_agent_type_under_two_models_separate() {
        let events = collect_agent_usage(&[
            agent_usage_event(
                1,
                Some(
                    r#"{"agentType":"general-purpose","model":"claude-haiku-5","outputTokens":10}"#,
                ),
            ),
            agent_usage_event(
                2,
                Some(
                    r#"{"agentType":"general-purpose","model":"claude-sonnet-5","outputTokens":20}"#,
                ),
            ),
        ]);

        let totals = aggregate_agent_usage(&events);

        assert_eq!(totals.len(), 2);
        let haiku_key = ("general-purpose".to_string(), "claude-haiku-5".to_string());
        let sonnet_key = ("general-purpose".to_string(), "claude-sonnet-5".to_string());
        assert_eq!(totals.get(&haiku_key).unwrap().output_tokens, 10);
        assert_eq!(totals.get(&sonnet_key).unwrap().output_tokens, 20);
    }

    #[test]
    fn aggregate_agent_usage_returns_empty_map_for_empty_input() {
        let totals = aggregate_agent_usage(&[]);
        assert!(totals.is_empty());
    }

    #[test]
    fn format_model_usage_returns_empty_vec_for_empty_map() {
        assert_eq!(
            format_model_usage(&ModelUsageMap::new()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn format_model_usage_omits_cost_column_when_none_present() {
        let mut map = ModelUsageMap::new();
        map.insert(
            "claude-fable-5".to_string(),
            ModelUsage {
                input_tokens: 146,
                output_tokens: 58564,
                cache_read_input_tokens: 6535803,
                cache_creation_input_tokens: 203983,
                cost_usd: None,
            },
        );

        let lines = format_model_usage(&map);
        assert_eq!(lines.len(), 1);
        assert!(!lines[0].contains('$'));
        assert!(lines[0].contains("out 58.6k"));
        assert!(lines[0].contains("in 146"));
        assert!(lines[0].contains("cache-read 6.5M"));
        assert!(lines[0].contains("cache-write 204.0k"));
    }

    #[test]
    fn format_model_usage_renders_cost_and_total_when_present() {
        let mut map = ModelUsageMap::new();
        map.insert(
            "claude-fable-5".to_string(),
            ModelUsage {
                input_tokens: 146,
                output_tokens: 58564,
                cache_read_input_tokens: 6535803,
                cache_creation_input_tokens: 203983,
                cost_usd: Some(12.996),
            },
        );
        map.insert(
            "claude-sonnet-5".to_string(),
            ModelUsage {
                input_tokens: 150,
                output_tokens: 30722,
                cache_read_input_tokens: 5400000,
                cache_creation_input_tokens: 191000,
                cost_usd: Some(2.81),
            },
        );

        let lines = format_model_usage(&map);
        assert_eq!(
            lines.len(),
            3,
            "expected two model lines plus a total: {lines:?}"
        );
        assert!(lines[0].starts_with("claude-fable-5"));
        assert!(lines[0].contains("$13.00"));
        assert!(lines[1].starts_with("claude-sonnet-5"));
        assert!(lines[1].contains("$2.81"));
        assert_eq!(lines[2], format!("total{}${:.2}", " ".repeat(12), 15.806));
    }

    #[test]
    fn format_agent_usage_returns_empty_vec_for_empty_map() {
        assert_eq!(
            format_agent_usage(&std::collections::BTreeMap::new()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn format_agent_usage_renders_single_row() {
        let mut totals = std::collections::BTreeMap::new();
        totals.insert(
            (
                "elixir-implementer".to_string(),
                "claude-sonnet-5".to_string(),
            ),
            AgentUsageTotals {
                invocations: 3,
                output_tokens: 1143,
                input_tokens: 2,
                cache_read_input_tokens: 87519,
                cache_creation_input_tokens: 3012,
                total_tool_use_count: 38,
                duration_ms: 193659,
            },
        );

        let lines = format_agent_usage(&totals);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("elixir-implementer"));
        assert!(!lines[0].contains("claude-sonnet-5"));
        assert!(lines[0].contains("3x"));
        assert!(lines[0].contains("out 1.1k"));
        assert!(lines[0].contains("in 2"));
        assert!(lines[0].contains("cache-read 87.5k"));
        assert!(lines[0].contains("cache-write 3.0k"));
        assert!(lines[0].contains("tools 38"));
    }

    #[test]
    fn format_agent_usage_renders_multiple_rows_aligned() {
        let mut totals = std::collections::BTreeMap::new();
        totals.insert(
            ("Explore".to_string(), "claude-haiku-5".to_string()),
            AgentUsageTotals {
                invocations: 1,
                output_tokens: 10,
                input_tokens: 1,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
                total_tool_use_count: 4,
                duration_ms: 100,
            },
        );
        totals.insert(
            (
                "elixir-implementer".to_string(),
                "claude-sonnet-5".to_string(),
            ),
            AgentUsageTotals {
                invocations: 3,
                output_tokens: 1143,
                input_tokens: 2,
                cache_read_input_tokens: 87519,
                cache_creation_input_tokens: 3012,
                total_tool_use_count: 38,
                duration_ms: 193659,
            },
        );

        let lines = format_agent_usage(&totals);
        assert_eq!(lines.len(), 2);
        // Both name columns padded to the same width before the detail text
        // starts, mirroring format_model_usage's alignment.
        let detail_start = |line: &str| line.find("1x,").or_else(|| line.find("3x,")).unwrap();
        assert_eq!(detail_start(&lines[0]), detail_start(&lines[1]));
    }

    #[test]
    fn format_agent_usage_expands_agent_type_with_two_models_into_two_lines() {
        let mut totals = std::collections::BTreeMap::new();
        totals.insert(
            ("general-purpose".to_string(), "claude-haiku-5".to_string()),
            AgentUsageTotals {
                invocations: 1,
                output_tokens: 10,
                input_tokens: 1,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
                total_tool_use_count: 2,
                duration_ms: 50,
            },
        );
        totals.insert(
            ("general-purpose".to_string(), "claude-sonnet-5".to_string()),
            AgentUsageTotals {
                invocations: 1,
                output_tokens: 20,
                input_tokens: 2,
                cache_read_input_tokens: 5,
                cache_creation_input_tokens: 1,
                total_tool_use_count: 6,
                duration_ms: 75,
            },
        );

        let lines = format_agent_usage(&totals);
        assert_eq!(lines.len(), 2, "expected one line per model: {lines:?}");
        assert!(lines[0].starts_with("general-purpose (claude-haiku-5)"));
        assert!(lines[1].starts_with("general-purpose (claude-sonnet-5)"));
    }

    #[test]
    fn format_event_detail_renders_usage_compactly() {
        let rendered = format_event_detail(
            "usage",
            Some(
                r#"{"models":{"claude-fable-5":{"outputTokens":89200},"claude-sonnet-5":{"outputTokens":30700}}}"#,
            ),
        );
        assert_eq!(
            rendered,
            Some("fable-5 89.2k out / sonnet-5 30.7k out".to_string())
        );
    }

    #[test]
    fn format_event_detail_returns_none_for_malformed_usage_detail() {
        assert_eq!(format_event_detail("usage", Some("not json")), None);
    }

    #[test]
    fn latest_audit_for_ticket_is_scoped_to_the_given_key() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());

        store.record_audit("PROJ-1", "ready", None).unwrap();
        store.record_audit("PROJ-2", "needs-work", None).unwrap();

        let audit = store
            .latest_audit_for_ticket("PROJ-2")
            .unwrap()
            .expect("expected an audit");
        assert_eq!(audit.ticket_key, "PROJ-2");
        assert_eq!(audit.verdict, "needs-work");
    }
}
