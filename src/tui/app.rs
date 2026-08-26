//! Pure Elm-style state and reducer for the TUI.
//!
//! [`App`] holds all UI state, [`Msg`] is every event the reducer can react
//! to, and [`update`] is the single place state transitions happen. No I/O
//! occurs here: [`Cmd`] values name the I/O the caller (`crate::tui::event`)
//! should perform next, and *Failed messages let callers feed I/O errors back
//! in without `update` ever needing to panic.

use std::collections::HashMap;

use crate::jira::client::RankAnchor;
use crate::runs::{RetroSeverity, RetroVerdict, RunStatus};
use crate::ticketing::provider::TicketQuery;
use crate::ticketing::types::{JiraUser, Transition};

/// A ticket as displayed on the board, derived from a
/// [`crate::ticketing::types::Issue`] plus the configured Jira base URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TicketSummary {
    /// Issue key, e.g. `PROJ-123`.
    pub key: String,
    /// One-line issue summary.
    pub summary: String,
    /// Current workflow status name.
    pub status: String,
    /// Browsable URL for the issue (`{base_url}/browse/{key}`).
    pub url: String,
    /// Plain-text description, as the provider renders it via
    /// [`crate::ticketing::provider::TicketProvider::description_text`].
    pub description: String,
    /// Status category key (`new`, `indeterminate`, `done`, or anything else
    /// Jira reports), used to order board columns.
    pub status_category: String,
    /// Display name of the ticket's assignee, or `None` if unassigned.
    pub assignee: Option<String>,
}

/// The board's assignee filter: which subset of tickets to show.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AssigneeFilter {
    /// The current user's open tickets (the original, unfiltered board).
    #[default]
    Me,
    /// Open tickets in the default project with no assignee.
    Unassigned,
    /// Every open ticket in the default project, regardless of assignee.
    Everyone,
    /// Open tickets in the default project assigned to a specific user.
    User(JiraUser),
}

impl AssigneeFilter {
    /// The human-readable label for this filter, used both in the picker
    /// list and the board's status line.
    pub fn label(&self) -> String {
        match self {
            AssigneeFilter::Me => "Me".to_string(),
            AssigneeFilter::Unassigned => "Unassigned".to_string(),
            AssigneeFilter::Everyone => "Everyone".to_string(),
            AssigneeFilter::User(user) => user.display_name.clone(),
        }
    }
}

/// Build the [`TicketQuery`] for `filter`, scoping project-wide filters to
/// `project_key`. [`AssigneeFilter::Me`] ignores `project_key` entirely,
/// preserving the board's original, unscoped query.
pub fn query_for_filter(filter: &AssigneeFilter, project_key: &str) -> TicketQuery {
    match filter {
        AssigneeFilter::Me => TicketQuery::MyOpen,
        AssigneeFilter::Unassigned => TicketQuery::Unassigned {
            project_key: project_key.to_string(),
        },
        AssigneeFilter::Everyone => TicketQuery::Everyone {
            project_key: project_key.to_string(),
        },
        AssigneeFilter::User(user) => TicketQuery::Assignee {
            project_key: project_key.to_string(),
            account_id: user.account_id.clone(),
        },
    }
}

/// One option in the board's assign picker: who to hand the selected card
/// to. Unlike [`AssigneeFilter`] there is no `Everyone`, and `Me` is an
/// action (assign to the current user) rather than a view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssignChoice {
    /// Assign the ticket to the current user, resolved via
    /// [`crate::ticketing::provider::TicketProvider::myself`] so it works
    /// under either backend (Jira accountId or GitHub login).
    Me,
    /// Clear the ticket's assignee.
    Unassign,
    /// Assign the ticket to a specific assignable user.
    User(JiraUser),
}

impl AssignChoice {
    /// The human-readable label for this choice, shown in the picker list.
    pub fn label(&self) -> String {
        match self {
            AssignChoice::Me => "Me".to_string(),
            AssignChoice::Unassign => "Unassign".to_string(),
            AssignChoice::User(user) => user.display_name.clone(),
        }
    }
}

/// One column of the sprint board: all tickets currently in a given status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    /// Status name shared by every ticket in this column.
    pub title: String,
    /// Tickets in this column, in fetch order.
    pub tickets: Vec<TicketSummary>,
}

/// Rank a status category for column ordering: `new` sorts first, then
/// `indeterminate`, then `done`, then anything unrecognized.
fn category_rank(category: &str) -> u8 {
    match category {
        "new" => 0,
        "indeterminate" => 1,
        "done" => 2,
        _ => 3,
    }
}

/// Group `tickets` into [`Column`]s, one per distinct status name.
///
/// Columns whose status name (case-insensitively) appears in `column_order`
/// sort first, in the order listed there. Every other column keeps the
/// default ordering -- status category rank (new, then indeterminate, then
/// done, then unknown categories), and alphabetically by status name within
/// a category -- and sorts after every listed column. Pass an empty slice to
/// leave every column on the default ordering. Tickets keep their relative
/// fetch order within a column.
pub fn group_into_columns(tickets: Vec<TicketSummary>, column_order: &[String]) -> Vec<Column> {
    let mut columns: Vec<Column> = Vec::new();

    for ticket in tickets {
        match columns.iter_mut().find(|c| c.title == ticket.status) {
            Some(column) => column.tickets.push(ticket),
            None => columns.push(Column {
                title: ticket.status.clone(),
                tickets: vec![ticket],
            }),
        }
    }

    let listed_rank = |title: &str| {
        column_order
            .iter()
            .position(|configured| configured.eq_ignore_ascii_case(title))
    };

    columns.sort_by(
        |a, b| match (listed_rank(&a.title), listed_rank(&b.title)) {
            (Some(rank_a), Some(rank_b)) => rank_a.cmp(&rank_b),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => {
                let rank_a = category_rank(&a.tickets[0].status_category);
                let rank_b = category_rank(&b.tickets[0].status_category);
                rank_a.cmp(&rank_b).then_with(|| a.title.cmp(&b.title))
            }
        },
    );

    columns
}

/// Which screen is currently shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Screen {
    /// The list of open tickets.
    #[default]
    Board,
    /// Full detail view of the ticket selected on the board.
    Detail,
    /// Menu of workflow transitions available on the selected ticket.
    TransitionMenu,
    /// The project's full open-ticket list in Jira backlog rank order,
    /// spanning every assignee, with grab-and-drop reordering.
    Rank,
    /// Live kanban of lane runs, entered via `tm runs watch`.
    Runs,
    /// Shipped tickets awaiting a retro verdict, entered via `R` from
    /// [`Screen::Board`]. See [`RetroRow`].
    Retro,
}

/// A run as displayed on the [`Screen::Runs`] kanban board, derived from a
/// [`crate::runs::RunSummary`]. Kept as its own type (rather than reusing
/// `RunSummary` directly) so the pure Elm core stays decoupled from the
/// store module's evolution; `crate::tui::event` maps between the two.
#[derive(Debug, Clone, PartialEq)]
pub struct RunCard {
    /// Row id.
    pub id: i64,
    /// Jira ticket key.
    pub ticket: String,
    /// Lane name.
    pub lane: String,
    /// Discriminates what kind of run this is, e.g. `lane`, `audit`,
    /// `create`; see [`crate::runs::StartRun::kind`].
    pub kind: String,
    /// Current status.
    pub status: crate::runs::RunStatus,
    /// Seconds since the run started.
    pub age_secs: i64,
    /// Seconds since the last heartbeat, or `None` if the run has ended.
    pub heartbeat_age_secs: Option<i64>,
    /// Kind of the most recent event recorded for this run, if any.
    pub last_event_kind: Option<String>,
    /// Seconds since the most recent event, if any.
    pub last_event_age_secs: Option<i64>,
    /// Whether the run is currently awaiting user input; see
    /// [`crate::runs::is_awaiting_input`].
    pub awaiting_input: bool,
    /// The run's latest checklist snapshot (see
    /// [`crate::runs::latest_checklist`]), if it has emitted one.
    pub checklist: Option<crate::runs::ChecklistState>,
}

/// One event in a [`RunDetail`]'s timeline, mirroring
/// [`crate::runs::RunEvent`].
#[derive(Debug, Clone, PartialEq)]
pub struct RunDetailEvent {
    /// When the event was recorded.
    pub at: String,
    /// Event kind, e.g. `tool_use` or `stop`.
    pub kind: String,
    /// Optional detail payload.
    pub detail: Option<String>,
}

/// Full detail for the floating window opened on [`Screen::Runs`], mirroring
/// [`crate::runs::Run`] plus its event timeline.
#[derive(Debug, Clone, PartialEq)]
pub struct RunDetail {
    /// Row id.
    pub id: i64,
    /// Jira ticket key.
    pub ticket: String,
    /// Lane name.
    pub lane: String,
    /// Discriminates what kind of run this is, e.g. `lane`, `audit`,
    /// `create`; see [`crate::runs::StartRun::kind`].
    pub kind: String,
    /// Current status.
    pub status: crate::runs::RunStatus,
    /// Filesystem path of the git worktree the run used.
    pub worktree: String,
    /// Branch checked out in the worktree, if known.
    pub branch: Option<String>,
    /// PID of the runner process, if known.
    pub pid: Option<u32>,
    /// `claude -p` session id, if recorded.
    pub session_id: Option<String>,
    /// Reported cost of the run in USD, if known.
    pub cost_usd: Option<f64>,
    /// Number of turns the run took, if known.
    pub num_turns: Option<i64>,
    /// URL of the pull request the run opened, if any.
    pub pr_url: Option<String>,
    /// Escalation text, set when `status` is [`crate::runs::RunStatus::Blocked`].
    pub blocker: Option<String>,
    /// When the run started.
    pub started_at: String,
    /// When the run ended, if it has.
    pub ended_at: Option<String>,
    /// The run's event timeline, oldest first.
    pub events: Vec<RunDetailEvent>,
    /// The run's latest checklist snapshot (see
    /// [`crate::runs::latest_checklist`]), if it has emitted one.
    pub checklist: Option<crate::runs::ChecklistState>,
    /// Counts of `tool` events by tool name (see
    /// [`crate::runs::tool_counts`]), sorted by count descending then name
    /// ascending. Empty when the run has emitted no `tool` events.
    pub tool_counts: Vec<(String, usize)>,
    /// Per-model token/cost usage to render, and whether it's the
    /// authoritative (has `costUSD`) or a live (running, no cost yet)
    /// snapshot. Prefers the run's `model_usage` column, falling back to
    /// the latest `usage` event while the run is still running (see
    /// [`crate::runs::latest_usage`]). `None` when neither is available.
    pub model_usage: Option<RunModelUsage>,
    /// Per-agent-type token usage lines (see
    /// [`crate::runs::format_agent_usage`]), computed from every
    /// `agent_usage` event the run has emitted (see
    /// [`crate::runs::collect_agent_usage`] and
    /// [`crate::runs::aggregate_agent_usage`]). Unlike `model_usage` there
    /// is no authoritative-vs-live distinction: agent usage always comes
    /// from events, never from a finish-time column. Empty when the run
    /// has emitted no `agent_usage` events.
    pub agent_usage: Vec<String>,
}

/// A [`RunDetail`]'s model usage breakdown, labeled so the UI can
/// distinguish the authoritative (post-finish, cost-bearing) snapshot from
/// a live in-progress one.
#[derive(Debug, Clone, PartialEq)]
pub struct RunModelUsage {
    /// Section label: `"Model usage"` when authoritative, `"Model usage
    /// (live)"` when sourced from a running run's latest `usage` event.
    pub label: &'static str,
    /// Formatted lines, per [`crate::runs::format_model_usage`].
    pub lines: Vec<String>,
}

/// A ticket's board-launched audit session state, per
/// `docs/plans/board-audits.md`'s "Board integration" design. Derived, never
/// stored: [`audit_indicator`] computes it fresh each poll from a live
/// `audit` window in the ticket's `tm-<scope>-<key>` tmux session and the ticket's
/// latest `kind = "audit"` run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditIndicator {
    /// A live `audit` window exists but no run has reached
    /// [`RunStatus::Running`] yet (or none was ever registered for it).
    Starting,
    /// The latest audit run is [`RunStatus::Running`], not currently
    /// awaiting input.
    Running,
    /// The latest audit run is [`RunStatus::Running`] and its most recent
    /// event was `await` (see [`crate::runs::is_awaiting_input`]).
    Waiting,
    /// The latest audit run finished successfully and its tmux session is
    /// still up: attachable aftermath. Once the session goes away the badge
    /// disappears entirely -- history lives in `tm runs`, not the board.
    Done,
    /// The latest audit run finished with an error and its tmux session is
    /// still up: attachable aftermath, same lifecycle as `Done`.
    Failed,
    /// The latest audit run ended abnormally, or its outcome couldn't be
    /// determined ([`RunStatus::Interrupted`]), and its tmux session is still
    /// up: attachable aftermath, same lifecycle as `Done`/`Failed`.
    Interrupted,
}

/// A ticket's full audit badge state on the board: its [`AuditIndicator`]
/// plus whether the action's tmux window is live.
///
/// `window_live` means exactly that: a live *window* for this action
/// (`audit`, or `bugbot` for `cleanup_status`) inside the ticket's shared
/// `tm-<scope>-<key>` session. It was called `has_session` until issue #2 phase 5,
/// from when each action owned a whole session; once one session came to hold
/// a ticket's entire action history, session existence stopped being a
/// liveness signal at all and the old name actively misled (see
/// [`crate::work::tmux::TmuxOps::list_windows`]).
///
/// Kept separate from `AuditIndicator` (rather than deriving `window_live`
/// back out of it) because attach-vs-launch (see [`Msg::AuditAction`]) must
/// key off that liveness alone: `Running`/`Waiting` can, in principle, be
/// reported for an audit run with no live window on this machine (e.g. one
/// launched and later killed on another host sharing the same runs DB), and
/// attaching is only ever possible when the window itself is live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditStatusEntry {
    /// The badge to render on the ticket's card.
    pub indicator: AuditIndicator,
    /// Whether the ticket has a live window for this action.
    pub window_live: bool,
}

/// Pure precedence rule for deriving a ticket's [`AuditIndicator`] from
/// whether its `audit` window is live (`window_live`) and its latest
/// `kind = "audit"` run's `(status, awaiting_input)`, if any exists.
/// `None` when neither a live window nor a run at [`RunStatus::Done`]/
/// [`RunStatus::Failed`] with a live window applies -- no badge.
///
/// Precedence, per `docs/plans/board-audits.md`'s "Board integration"
/// design:
/// 1. Running + awaiting input -> [`AuditIndicator::Waiting`].
/// 2. Running, not awaiting -> [`AuditIndicator::Running`].
/// 3. No run recorded at all, but the window is live -> [`AuditIndicator::Starting`].
/// 4. A finished (`Done`/`Failed`/`Interrupted`) run *and* a still-live
///    window -> the matching terminal indicator (attachable aftermath).
/// 5. Otherwise: no badge.
pub fn audit_indicator(
    window_live: bool,
    run: Option<(RunStatus, bool)>,
) -> Option<AuditIndicator> {
    match run {
        Some((RunStatus::Running, true)) => Some(AuditIndicator::Waiting),
        Some((RunStatus::Running, false)) => Some(AuditIndicator::Running),
        Some((RunStatus::Done, _)) if window_live => Some(AuditIndicator::Done),
        Some((RunStatus::Failed, _)) if window_live => Some(AuditIndicator::Failed),
        Some((RunStatus::Interrupted, _)) if window_live => Some(AuditIndicator::Interrupted),
        None if window_live => Some(AuditIndicator::Starting),
        _ => None,
    }
}

/// A ticket's board-launched lane run state, per
/// `docs/plans/board-lane-runs.md`. Derived, never stored:
/// [`lane_run_indicator`] computes it fresh each poll from the ticket's
/// latest `kind = "lane"` run plus the pending-launch set. Unlike
/// [`AuditIndicator`] there is no tmux-session liveness input -- the
/// indicator comes purely from the run row (which outlives the process), so
/// terminal badges persist until a newer run replaces them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunIndicator {
    /// A launcher child is in flight (no run row yet -- `prepare_run_lane`
    /// creates none until preflight succeeds).
    Starting,
    /// The latest lane run is [`RunStatus::Queued`] or [`RunStatus::Running`],
    /// not currently awaiting input.
    Running,
    /// The latest lane run is [`RunStatus::Running`] and its most recent
    /// event was `await` (see [`crate::runs::is_awaiting_input`]), or it is
    /// [`RunStatus::Blocked`].
    Waiting,
    /// The latest lane run finished successfully ([`RunStatus::Review`] or
    /// [`RunStatus::Done`]).
    Done,
    /// The latest lane run finished with an error.
    Failed,
    /// The latest lane run ended abnormally, or its outcome couldn't be
    /// determined ([`RunStatus::Interrupted`]) — distinct from `Failed`
    /// because the agent never actually reported failure.
    Interrupted,
}

/// Pure precedence rule for deriving a ticket's [`RunIndicator`] from
/// whether a launcher child is in flight for it (`pending`) and its latest
/// `kind = "lane"` run's `(status, awaiting_input)`, if any exists.
///
/// A run row, once it exists, is fresher truth than the pending flag -- it
/// reflects what `prepare_run_lane` actually recorded, whereas `pending`
/// only covers the window before preflight succeeds. So `pending` wins as
/// [`RunIndicator::Starting`] only when there is no run row at all; once one
/// exists, its status decides the indicator regardless of `pending`.
///
/// Mapping, per `docs/plans/board-lane-runs.md`'s "Indicator mapping" table:
/// 1. `Running` + awaiting input, or `Blocked` -> [`RunIndicator::Waiting`].
/// 2. `Queued`/`Running`, not awaiting -> [`RunIndicator::Running`].
/// 3. `Review`/`Done` -> [`RunIndicator::Done`].
/// 4. `Failed` -> [`RunIndicator::Failed`].
/// 5. `Interrupted` -> [`RunIndicator::Interrupted`].
/// 6. No run recorded at all, but a launch is pending ->
///    [`RunIndicator::Starting`].
/// 7. Otherwise: no badge.
pub fn lane_run_indicator(pending: bool, run: Option<(RunStatus, bool)>) -> Option<RunIndicator> {
    match run {
        Some((RunStatus::Running, true)) => Some(RunIndicator::Waiting),
        Some((RunStatus::Blocked, _)) => Some(RunIndicator::Waiting),
        Some((RunStatus::Queued, _)) => Some(RunIndicator::Running),
        Some((RunStatus::Running, false)) => Some(RunIndicator::Running),
        Some((RunStatus::Review, _)) => Some(RunIndicator::Done),
        Some((RunStatus::Done, _)) => Some(RunIndicator::Done),
        Some((RunStatus::Failed, _)) => Some(RunIndicator::Failed),
        Some((RunStatus::Interrupted, _)) => Some(RunIndicator::Interrupted),
        None if pending => Some(RunIndicator::Starting),
        None => None,
    }
}

/// A ticket's PR bot-watch state on the board, per
/// `docs/plans/bugbot-watch.md`'s "Board integration" design. Derived, never
/// stored: [`bot_watch_indicator`] computes it fresh each poll from the
/// ticket's latest `kind = "review-watch"` run.
///
/// Like [`RunIndicator`] (and unlike [`AuditIndicator`]) there is no
/// tmux-session liveness input: `tm pr watch`'s poll loop is a headless
/// background process, so the run row is the only source of truth and a
/// terminal badge persists until a newer watcher run supersedes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BotWatchIndicator {
    /// The latest watcher run is [`RunStatus::Running`]: the PR is up and the
    /// bots haven't all reviewed yet.
    Watching,
    /// The latest watcher run finished [`RunStatus::Review`]: every bot has
    /// reviewed and left unresolved findings, so a cleanup session is worth
    /// launching. The loud, act-on-me state.
    Ready,
    /// The latest watcher run finished [`RunStatus::Done`]: the bots reviewed
    /// with nothing left unresolved (or the PR merged/closed first) -- nothing
    /// to clean up.
    Clean,
    /// The latest watcher run finished [`RunStatus::Failed`]: it gave up (bad
    /// `gh`, or the wall-clock timeout) rather than reaching a verdict.
    Failed,
}

/// Pure mapping from a ticket's latest `kind = "review-watch"` run status to
/// its [`BotWatchIndicator`], per `docs/plans/bugbot-watch.md`'s "Board
/// integration" design. `None` (no badge) when there is no watcher run at all,
/// or when its status is one a watcher never reports
/// ([`RunStatus::Queued`]/[`RunStatus::Blocked`]).
pub fn bot_watch_indicator(run: Option<RunStatus>) -> Option<BotWatchIndicator> {
    match run {
        Some(RunStatus::Running) => Some(BotWatchIndicator::Watching),
        Some(RunStatus::Review) => Some(BotWatchIndicator::Ready),
        Some(RunStatus::Done) => Some(BotWatchIndicator::Clean),
        Some(RunStatus::Failed) => Some(BotWatchIndicator::Failed),
        _ => None,
    }
}

/// One choice in the floating picker [`Msg::OpenBrowserAction`] opens when
/// the selected ticket has both a Jira issue and an open GitHub pull request.
/// Built once, in [`browser_options_resolved`], from the ticket's own `url`
/// plus the [`crate::github::pr::PrInfo`] [`Cmd::ResolvePrForTicket`]
/// resolved -- never rebuilt by the picker's up/down/select handlers, which
/// only move `App::browser_picker_selected` or read `url()` off of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserPickerOption {
    /// Open the ticket's Jira issue.
    Jira {
        /// Issue key, shown in the picker as `Jira (<key>)`.
        key: String,
        /// Browsable Jira URL.
        url: String,
    },
    /// Open the associated GitHub pull request.
    GitHub {
        /// Pull request number, shown in the picker as `GitHub (#<number>)`.
        number: u64,
        /// Browsable GitHub URL.
        url: String,
    },
}

impl BrowserPickerOption {
    /// The label to render for this option in the picker list.
    pub fn label(&self) -> String {
        match self {
            BrowserPickerOption::Jira { key, .. } => format!("Jira ({key})"),
            BrowserPickerOption::GitHub { number, .. } => format!("GitHub (#{number})"),
        }
    }

    /// The URL [`Msg::BrowserPickerSelect`] opens for this option.
    pub fn url(&self) -> &str {
        match self {
            BrowserPickerOption::Jira { url, .. } | BrowserPickerOption::GitHub { url, .. } => url,
        }
    }
}

/// A shipped ticket's latest `kind = "lane"` run, as much of it as
/// [`Screen::Retro`] shows: cost and model mix. Kept separate from
/// [`crate::runs::Run`] for the same reason [`RunCard`]/[`RunDetail`] are --
/// decoupling the pure Elm core from the store module's evolution.
///
/// A ticket with `run: None` on its [`RetroRow`] never had a lane run at
/// all (shipped manually, or through some other path); that's a distinct
/// case from *this* struct's fields being `None` (a lane run exists but
/// hasn't recorded a cost or model breakdown yet, e.g. still running) --
/// [`RetroRow::run`] is what carries the "no run at all" case, not this
/// type. Rendering must keep those two states visually distinct: a ticket
/// with no run is not the same as a ticket whose run cost `$0.00`.
#[derive(Debug, Clone, PartialEq)]
pub struct RetroRunInfo {
    /// The run's reported cost in USD, `None` if not yet recorded (e.g. the
    /// run is still in progress).
    pub cost_usd: Option<f64>,
    /// One-line model-mix summary (see
    /// [`crate::runs::format_model_usage_compact`]), `None` if the run has
    /// no per-model usage recorded yet.
    pub model_summary: Option<String>,
}

/// One row on [`Screen::Retro`]: a shipped ticket (Jira status category
/// `Done`) with no recorded retro verdict yet, per
/// [`crate::ticketing::provider::TicketQuery::ShippedAwaitingRetro`].
#[derive(Debug, Clone, PartialEq)]
pub struct RetroRow {
    /// Issue key, e.g. `PROJ-123`.
    pub key: String,
    /// One-line issue summary.
    pub summary: String,
    /// Browsable Jira URL, for `o`/`O`.
    pub url: String,
    /// Its latest `kind = "lane"` run's cost/model info, or `None` if it has
    /// never had one (e.g. the work shipped manually) -- see
    /// [`RetroRunInfo`]'s doc comment for why that's a distinct state from
    /// this struct's fields being unset.
    pub run: Option<RetroRunInfo>,
}

/// The severities [`Msg::RetroSeverityPickerUp`]/`Down` cycle through, in
/// display order.
pub const RETRO_SEVERITIES: [RetroSeverity; 3] = [
    RetroSeverity::Minor,
    RetroSeverity::Major,
    RetroSeverity::Critical,
];

/// The fixed column order for [`Screen::Runs`]'s kanban board. All seven
/// columns always render, even when empty.
pub const RUN_COLUMNS: [crate::runs::RunStatus; 7] = [
    crate::runs::RunStatus::Queued,
    crate::runs::RunStatus::Running,
    crate::runs::RunStatus::Blocked,
    crate::runs::RunStatus::Review,
    crate::runs::RunStatus::Done,
    crate::runs::RunStatus::Failed,
    crate::runs::RunStatus::Interrupted,
];

/// All state needed to render and drive the TUI.
#[derive(Debug, Clone, Default)]
pub struct App {
    /// Board columns, one per status, in display order.
    pub columns: Vec<Column>,
    /// Index into `columns` of the currently selected column. Always clamped
    /// into bounds (`0` when `columns` is empty).
    pub selected_col: usize,
    /// Index into the selected column's tickets of the currently selected
    /// ticket. Always clamped into bounds (`0` when the column is empty).
    pub selected_row: usize,
    /// The screen currently shown.
    pub screen: Screen,
    /// Transitions available on the selected ticket, populated when
    /// [`Screen::TransitionMenu`] opens.
    pub transitions: Vec<Transition>,
    /// Index into `transitions` of the currently highlighted transition.
    pub transition_selected: usize,
    /// Scroll offset into the detail view's body text.
    pub detail_scroll: u16,
    /// Feedback from the last action or error, shown in the status bar.
    pub status_line: String,
    /// Whether the help overlay is shown.
    pub show_help: bool,
    /// The configured default Jira project key, used to scope every
    /// [`AssigneeFilter`] other than [`AssigneeFilter::Me`].
    pub project_key: String,
    /// The current repo's [`crate::config::BackendIdentity::session_slug`],
    /// qualifying every ticket-session name this board builds (see
    /// [`crate::work::naming::ticket_session_name`]) so same-numbered
    /// tickets in different repos never share a tmux session (GitHub issue
    /// #10). Set from `TuiDeps` at construction, like `project_key`.
    pub session_slug: String,
    /// Configured board column order (status names, case-insensitive),
    /// from [`crate::config::Config::board_column_order`]. Listed columns
    /// sort first, in this order; unlisted columns keep the default
    /// category-then-name ordering and sort after. Empty when unconfigured.
    pub board_column_order: Vec<String>,
    /// The board's active assignee filter.
    pub filter: AssigneeFilter,
    /// Assignable users for `project_key`, fetched lazily the first time the
    /// filter picker opens and cached for the rest of the session. `None`
    /// until a fetch has succeeded at least once.
    pub assignable_users: Option<Vec<JiraUser>>,
    /// Whether the assignee filter picker overlay is shown.
    pub show_filter_picker: bool,
    /// Index into the picker's option list of the currently highlighted
    /// option.
    pub filter_picker_selected: usize,
    /// Error from the last failed assignable-users fetch, shown in the
    /// picker until the next successful fetch.
    pub filter_picker_error: Option<String>,
    /// Whether the assign picker overlay is shown.
    pub show_assign_picker: bool,
    /// Key of the ticket the assign picker was opened on. `Some` whenever
    /// the picker is open; captured at open time so the picker stays aimed
    /// at that card no matter what the board does underneath it.
    pub assign_picker_key: Option<String>,
    /// Index into [`App::assign_options`] of the currently highlighted
    /// option.
    pub assign_picker_selected: usize,
    /// Error from the last failed assignable-users fetch, shown in the
    /// assign picker until the next successful fetch. Kept separate from
    /// [`App::filter_picker_error`] so each picker renders its own state.
    pub assign_picker_error: Option<String>,
    /// [`Screen::Rank`]'s ticket list, in Jira backlog rank order. Kept
    /// entirely separate from `columns` so leaving the rank screen never
    /// requires refetching (or clobbers) the board.
    pub rank_tickets: Vec<TicketSummary>,
    /// Index into `rank_tickets` of the currently highlighted row.
    pub rank_selected: usize,
    /// The original index of the currently grabbed ticket in `rank_tickets`,
    /// or `None` if nothing is grabbed. Used to detect a no-op drop (dropped
    /// back at its starting position) and to restore `rank_selected` on
    /// cancel.
    pub rank_grab_origin: Option<usize>,
    /// A snapshot of `rank_tickets` taken at grab time, restored verbatim if
    /// the grab is cancelled. `None` whenever nothing is grabbed.
    pub rank_snapshot: Option<Vec<TicketSummary>>,
    /// Set when the event loop should exit.
    pub quit: bool,
    /// [`Screen::Runs`]'s cards, in the order [`RunStore::list_runs`] returns
    /// them.
    pub runs: Vec<RunCard>,
    /// Index into [`RUN_COLUMNS`] of the currently selected column.
    pub runs_selected_col: usize,
    /// Index into the selected run column's cards of the currently selected
    /// card. Always clamped into bounds (`0` when the column is empty).
    pub runs_selected_row: usize,
    /// Whether the run detail floating window is shown.
    pub show_run_detail: bool,
    /// Detail for the run shown in the floating window, `None` while it's
    /// still loading.
    pub run_detail: Option<RunDetail>,
    /// Scroll offset into the run detail window's event timeline.
    pub run_detail_scroll: u16,
    /// Number of [`Msg::Tick`]s processed since the event loop started, used
    /// to throttle [`Screen::Runs`]'s polling/periodic reaping and
    /// [`Screen::Board`]'s audit-status polling. Shared across both screens
    /// since only one is ever active at a time.
    pub watch_tick: u64,
    /// Per-ticket audit badge state for [`Screen::Board`], keyed by ticket
    /// key. Populated by [`Cmd::LoadAuditStatus`], polled every 8th
    /// [`Msg::Tick`] (~2s at the 250ms poll interval) plus once at startup.
    /// Empty when the runs DB is unavailable or no ticket has ever had an
    /// audit session.
    pub audit_status: HashMap<String, AuditStatusEntry>,
    /// Whether the lane picker overlay is shown (see [`Msg::LaneRunAction`]).
    pub show_lane_picker: bool,
    /// Index into `lane_names` of the currently highlighted lane in the
    /// picker.
    pub lane_picker_selected: usize,
    /// Configured `[work.lanes]` names, in `BTreeMap` order. Threaded from
    /// config at construction (see [`App::with_lane_names`]); empty until
    /// wired up, which disables `w`'s launch entirely (see
    /// [`Msg::LaneRunAction`]'s "no lanes configured" case).
    pub lane_names: Vec<String>,
    /// Count of configured `[work.lanes]` entries hidden from `lane_names`
    /// because their repo's resolved backend identity doesn't match the
    /// board's own repo (see [`crate::config::compatible_lane_names`]),
    /// threaded from config at construction alongside `lane_names` (see
    /// [`App::with_hidden_lane_count`]). Used only for status-line/picker
    /// messaging so a backend mismatch reads as "hidden", not "unconfigured"
    /// — see GitHub issue #5 phase 2:
    /// `docs/plans/issue-5-lane-backend-routing.md`.
    pub hidden_lane_count: usize,
    /// Ticket keys with a lane-run launcher child currently in flight (no
    /// run row recorded yet). Populated by [`Msg::LaneRunAction`]/
    /// [`Msg::LanePickerSelect`], cleared by [`Msg::LaneRunLaunchResult`].
    pub pending_lane_launches: std::collections::HashSet<String>,
    /// Per-ticket lane-run badge state for [`Screen::Board`], keyed by ticket
    /// key. Populated by [`Cmd::LoadLaneRunStatus`], polled every 8th
    /// [`Msg::Tick`] (~2s at the 250ms poll interval), same cadence as
    /// `audit_status`. Empty when the runs DB is unavailable or no ticket has
    /// ever had a lane run.
    pub lane_run_status: HashMap<String, RunIndicator>,
    /// Per-ticket PR bot-watch badge state for [`Screen::Board`], keyed by
    /// ticket key. Populated by [`Cmd::LoadBotWatchStatus`], polled every 8th
    /// [`Msg::Tick`] (~2s at the 250ms poll interval), same cadence as
    /// `audit_status`/`lane_run_status`. Empty when the runs DB is unavailable
    /// or no ticket has ever had a `tm pr watch` run.
    pub bot_watch_status: HashMap<String, BotWatchIndicator>,
    /// Per-ticket bugbot-cleanup session badge state for [`Screen::Board`],
    /// keyed by ticket key. Populated by [`Cmd::LoadCleanupStatus`] on the same
    /// cadence, and derived by the *same* [`audit_indicator`] rule as
    /// `audit_status` (a cleanup session is a tmux-hosted interactive session
    /// too -- see `docs/plans/bugbot-watch.md`'s "Board integration"), just
    /// over `kind = "bugbot-cleanup"` runs and `bugbot` windows.
    pub cleanup_status: HashMap<String, AuditStatusEntry>,
    /// Ticket keys with a `tm pr watch` launcher child currently in flight (no
    /// `review-watch` run row recorded yet). Populated by [`Msg::BotsAction`],
    /// cleared by [`Msg::BotWatchLaunchResult`]. Rendered as a starting-style
    /// `bots:` badge, the bot-watch counterpart of `pending_lane_launches`.
    pub pending_bot_watch_launches: std::collections::HashSet<String>,
    /// Whether the browser picker overlay is shown (see
    /// [`Msg::OpenBrowserAction`]).
    pub show_browser_picker: bool,
    /// Index into `browser_picker_options` of the currently highlighted
    /// option.
    pub browser_picker_selected: usize,
    /// The browser picker's option list -- Jira plus the resolved GitHub PR
    /// -- built by [`browser_options_resolved`] once [`Cmd::ResolvePrForTicket`]
    /// reports a PR was found. Empty whenever the picker is closed.
    pub browser_picker_options: Vec<BrowserPickerOption>,
    /// [`Screen::Retro`]'s ticket list: shipped tickets awaiting a retro
    /// verdict, newest-resolved first. Kept entirely separate from
    /// `columns`/`rank_tickets`, same reasoning as those two.
    pub retro_tickets: Vec<RetroRow>,
    /// Index into `retro_tickets` of the currently highlighted row.
    pub retro_selected: usize,
    /// Whether the defect-severity picker overlay is shown (see
    /// [`Msg::RetroDefectStart`]).
    pub show_retro_severity_picker: bool,
    /// Index into [`RETRO_SEVERITIES`] of the currently highlighted severity.
    pub retro_severity_selected: usize,
    /// Whether the optional note-entry overlay is shown, following severity
    /// selection in the defect flow.
    pub show_retro_note_entry: bool,
    /// The note's in-progress text, built up character by character while
    /// [`App::show_retro_note_entry`] is set. Submitting with this empty
    /// records no note at all, rather than an empty string.
    pub retro_note_draft: String,
    /// The ticket key a defect flow (severity picker, then note entry) is
    /// in progress for. Captured at [`Msg::RetroDefectStart`] rather than
    /// re-read off `retro_selected` at submit time, so the flow still
    /// targets the right ticket even if -- not that anything currently lets
    /// it -- the underlying list were to change out from under it.
    pub retro_action_key: Option<String>,
    /// The severity chosen by [`Msg::RetroSeverityPickerSelect`], carried
    /// through the note-entry step to [`Msg::RetroNoteSubmit`].
    pub retro_pending_severity: Option<RetroSeverity>,
}

impl App {
    /// An app with no tickets, showing the board.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set `lane_names`, for threading the board's repo-compatible
    /// `[work.lanes]` names into the board at construction (see
    /// [`App::lane_names`]'s doc comment). Called from `tui::event::run`.
    pub fn with_lane_names(mut self, lane_names: Vec<String>) -> Self {
        self.lane_names = lane_names;
        self
    }

    /// Set `hidden_lane_count` (see that field's doc comment), for threading
    /// how many configured lanes were filtered out for a backend mismatch
    /// into the board at construction, alongside [`Self::with_lane_names`].
    pub fn with_hidden_lane_count(mut self, hidden_lane_count: usize) -> Self {
        self.hidden_lane_count = hidden_lane_count;
        self
    }

    /// The currently selected ticket, if any.
    pub fn selected_ticket(&self) -> Option<&TicketSummary> {
        self.columns
            .get(self.selected_col)?
            .tickets
            .get(self.selected_row)
    }

    /// The currently highlighted ticket on [`Screen::Rank`], if any.
    pub fn rank_selected_ticket(&self) -> Option<&TicketSummary> {
        self.rank_tickets.get(self.rank_selected)
    }

    /// Whether a ticket is currently grabbed on [`Screen::Rank`].
    pub fn is_rank_grabbed(&self) -> bool {
        self.rank_grab_origin.is_some()
    }

    /// The currently highlighted row on [`Screen::Retro`], if any.
    pub fn retro_selected_ticket(&self) -> Option<&RetroRow> {
        self.retro_tickets.get(self.retro_selected)
    }

    /// The assignee filter picker's options, in display order: `Me`,
    /// `Unassigned`, `Everyone`, then each cached assignable user.
    pub fn filter_options(&self) -> Vec<AssigneeFilter> {
        let mut options = vec![
            AssigneeFilter::Me,
            AssigneeFilter::Unassigned,
            AssigneeFilter::Everyone,
        ];
        if let Some(users) = &self.assignable_users {
            options.extend(users.iter().cloned().map(AssigneeFilter::User));
        }
        options
    }

    /// The assign picker's option list: `Me` and `Unassign` always, then
    /// every cached assignable user (the same lazily fetched
    /// [`App::assignable_users`] cache the filter picker uses).
    pub fn assign_options(&self) -> Vec<AssignChoice> {
        let mut options = vec![AssignChoice::Me, AssignChoice::Unassign];
        if let Some(users) = &self.assignable_users {
            options.extend(users.iter().cloned().map(AssignChoice::User));
        }
        options
    }

    /// The run cards in `self.runs` whose status is `RUN_COLUMNS[col]`,
    /// preserving `self.runs`' order. Empty (rather than panicking) if `col`
    /// is out of bounds.
    pub fn runs_in_col(&self, col: usize) -> Vec<&RunCard> {
        let Some(status) = RUN_COLUMNS.get(col) else {
            return Vec::new();
        };
        self.runs.iter().filter(|c| c.status == *status).collect()
    }

    /// The currently highlighted run card on [`Screen::Runs`], if any.
    pub fn selected_run_card(&self) -> Option<&RunCard> {
        self.runs_in_col(self.runs_selected_col)
            .into_iter()
            .nth(self.runs_selected_row)
    }
}

/// Every event the reducer can react to.
#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    /// Move the current selection/scroll up.
    Up,
    /// Move the current selection/scroll down.
    Down,
    /// Move the selected board column left. Ignored on non-board screens.
    Left,
    /// Move the selected board column right. Ignored on non-board screens.
    Right,
    /// Activate the current selection.
    Enter,
    /// Go back to the previous screen, or quit from the board.
    Back,
    /// Reload the ticket list.
    Refresh,
    /// Open the selected ticket's URL in a browser directly, with no PR
    /// lookup. Bound to `O` on every screen, and to `o` on every screen
    /// *except* [`Screen::Board`] (where `o` instead maps to
    /// [`Msg::OpenBrowserAction`]) -- see [`crate::tui::keymap::map_key`]'s
    /// doc comment for the full split.
    OpenInBrowser,
    /// The `o` key was pressed on [`Screen::Board`] for the selected ticket:
    /// resolve whether it has an open GitHub PR (via
    /// [`Cmd::ResolvePrForTicket`]) before deciding whether to show the
    /// browser picker or open Jira directly, per
    /// [`browser_options_resolved`]'s precedence. A no-op when no ticket is
    /// selected, mirroring [`Msg::OpenInBrowser`].
    OpenBrowserAction,
    /// [`Cmd::ResolvePrForTicket`] finished: `pr` is `Some` when an open
    /// GitHub PR was found for `key`, `None` otherwise (no PR yet, or `gh`/the
    /// repo couldn't be resolved -- degrading to "no PR" rather than an error,
    /// per [`crate::tui::event::TuiDeps`]'s leniency stance). `jira_url` is
    /// carried through from the keypress rather than re-read off the ticket,
    /// so this resolves correctly even if the selection has moved on by the
    /// time `gh` answers.
    BrowserOptionsResolved {
        /// Ticket key the lookup was for.
        key: String,
        /// The ticket's Jira URL, captured at keypress time.
        jira_url: String,
        /// The resolved PR, if any.
        pr: Option<crate::github::pr::PrInfo>,
        /// A status-line note to surface alongside the resolution, if the
        /// lookup degraded in a way the user should know about (currently:
        /// the bounded `gh pr list` call timed out -- see
        /// [`crate::tui::event::resolve_pr_for_ticket`]). `None` for the
        /// ordinary "found a PR" / "no PR yet" outcomes, which need no
        /// explanation beyond the picker or the direct Jira open itself.
        note: Option<String>,
    },
    /// Move the browser picker's highlighted option up.
    BrowserPickerUp,
    /// Move the browser picker's highlighted option down.
    BrowserPickerDown,
    /// Open the browser picker's highlighted option's URL, and close the
    /// picker.
    BrowserPickerSelect,
    /// Close the browser picker without opening anything.
    BrowserPickerClose,
    /// Toggle the help overlay.
    ToggleHelp,
    /// Quit the application.
    Quit,
    /// Open the assignee filter picker overlay. Only meaningful on
    /// [`Screen::Board`]; [`crate::tui::keymap::map_key`] only ever emits
    /// this from there.
    OpenFilterPicker,
    /// Move the filter picker's highlighted option up.
    FilterPickerUp,
    /// Move the filter picker's highlighted option down.
    FilterPickerDown,
    /// Apply the filter picker's highlighted option and close the picker.
    FilterPickerSelect,
    /// Close the filter picker without changing the active filter.
    FilterPickerClose,
    /// Assignable users for the picker finished loading.
    AssignableUsersLoaded(Vec<JiraUser>),
    /// Assignable users failed to load.
    AssignableUsersFailed(String),
    /// Open the assign picker overlay on the selected ticket. Only
    /// meaningful on [`Screen::Board`]; a no-op when nothing is selected.
    OpenAssignPicker,
    /// Move the assign picker's highlighted option up.
    AssignPickerUp,
    /// Move the assign picker's highlighted option down.
    AssignPickerDown,
    /// Apply the assign picker's highlighted choice and close the picker.
    AssignPickerSelect,
    /// Close the assign picker without changing the ticket's assignee.
    AssignPickerClose,
    /// The outcome of a successful [`Cmd::AssignTicket`]: `key` is now
    /// assigned to `assignee` (a display name), or unassigned when `None`.
    /// Updates the card in place -- assignee never changes a card's column,
    /// so no refetch is needed.
    AssignApplied {
        /// Key of the ticket that was assigned.
        key: String,
        /// Display name of the new assignee, or `None` when unassigned.
        assignee: Option<String>,
    },
    /// [`Cmd::AssignTicket`] failed; the message goes to the status line.
    AssignFailed(String),
    /// The ticket list finished loading.
    TicketsLoaded(Vec<TicketSummary>),
    /// The ticket list failed to load.
    TicketsFailed(String),
    /// A ticket search hit [`crate::jira::client::MAX_SEARCH_PAGES`] with
    /// more matches still unfetched, so the screen is showing a truncated
    /// list. Emitted alongside (after) the load message rather than folded
    /// into it: truncation is a warning about the results, not a different
    /// kind of result, and every loaded-list handler stays untouched.
    SearchTruncated {
        /// How many tickets were actually fetched and are on screen.
        shown: usize,
    },
    /// Transitions for the selected ticket finished loading.
    TransitionsLoaded(Vec<Transition>),
    /// Transitions for the selected ticket failed to load.
    TransitionsFailed(String),
    /// A transition was successfully applied.
    TransitionApplied {
        /// Key of the ticket that was transitioned.
        key: String,
        /// New status name after the transition.
        status: String,
        /// New status category key after the transition, used to regroup
        /// the ticket into the right board column.
        status_category: String,
    },
    /// A transition failed to apply.
    TransitionFailed(String),
    /// Open the priority (stack-rank) screen. Only meaningful on
    /// [`Screen::Board`]; [`crate::tui::keymap::map_key`] only ever emits
    /// this from there.
    OpenRank,
    /// The rank screen's ticket list finished loading.
    RankTicketsLoaded(Vec<TicketSummary>),
    /// The rank screen's ticket list failed to load.
    RankTicketsFailed(String),
    /// Grab the highlighted ticket on the rank screen, or drop it if it's
    /// already grabbed. A no-op when the rank list is empty.
    RankGrabToggle,
    /// A rank reorder was successfully applied.
    RankApplied(String),
    /// A rank reorder failed to apply.
    RankFailed(String),
    /// A poll timeout elapsed with no key pressed. Only meaningful on
    /// [`Screen::Runs`]; ignored on every other screen.
    Tick,
    /// The runs kanban board finished loading.
    RunsLoaded(Vec<RunCard>),
    /// The runs kanban board failed to load.
    RunsFailed(String),
    /// The run detail window's data finished loading.
    RunDetailLoaded(Box<RunDetail>),
    /// The run detail window's data failed to load. Closes the overlay if
    /// nothing had loaded yet (`run_detail == None`); a refresh failure
    /// after a successful load leaves the loaded content up.
    RunDetailFailed(String),
    /// The `v` key was pressed on [`Screen::Board`] for the selected ticket:
    /// open the run detail overlay on its latest run (any `kind`), per
    /// `docs/plans/board-run-detail.md`'s "Decisions" section. A no-op when
    /// no ticket is selected.
    ViewRunAction,
    /// The `L` key was pressed on [`Screen::Board`] for the selected ticket:
    /// open its latest run's log file (see [`Cmd::ViewLogs`]). A no-op when
    /// no ticket is selected.
    ViewLogsAction,
    /// The outcome of [`Cmd::ViewLogs`], already rendered to a
    /// human-readable status-line message -- mirrors
    /// [`Msg::AuditActionResult`]'s single-string-variant shape.
    LogsActionResult(String),
    /// The `V` key was pressed on [`Screen::Board`] for the selected ticket:
    /// open its worktree in `vdiff` for review, per
    /// `docs/plans/board-vdiff-review-loop.md`'s "Decisions" section. A
    /// no-op when no ticket is selected; degrades to a status-line message
    /// (via [`view_diff_action`]) when the ticket has no lane run, rather
    /// than emitting [`Cmd::ViewDiff`] with nothing to launch.
    ViewDiffAction,
    /// The outcome of [`Cmd::ViewDiff`], already rendered to a
    /// human-readable status-line message -- mirrors
    /// [`Msg::LogsActionResult`]'s single-string-variant shape.
    DiffActionResult(String),
    /// The `F` key was pressed on [`Screen::Board`] for the selected ticket:
    /// dispatch a fix pass over the review comments captured for it in
    /// `vdiff`, per `docs/plans/board-vdiff-review-loop.md`. A no-op when no
    /// ticket is selected; degrades to a status-line message (via
    /// [`review_fix_action`]) when the ticket has no lane run, rather than
    /// spawning `tm review fix` with no worktree to run it in.
    ReviewFixAction,
    /// The outcome of [`Cmd::LaunchReviewFix`] for `key`: a watched-child
    /// spawn through the same [`crate::tui::launcher::LaneLauncher`] seam
    /// [`Msg::LaneRunLaunchResult`]/[`Msg::BotWatchLaunchResult`] use.
    /// `Ok(())` means `tm review fix <key>` exited zero (the fix pass is
    /// dispatched and detached); `Err` carries its stderr, e.g. "no comments
    /// captured" or "no lane run for `<key>`".
    ReviewFixLaunchResult {
        /// Ticket key the fix pass was dispatched for.
        key: String,
        /// Outcome of the watched launcher child.
        result: Result<(), String>,
    },
    /// A reap pass completed, having reaped `0` reports as a no-op status
    /// line.
    RunsReaped(usize),
    /// The `a` key was pressed on [`Screen::Board`] for the selected ticket:
    /// attach to its `tm-<scope>-<key>` session if its `audit` window is live,
    /// otherwise launch a new one. See [`audit_action`].
    AuditAction,
    /// The `s` key was pressed on [`Screen::Board`]: attach to the selected
    /// ticket's `tm-<scope>-<key>` session, whatever is in it. See [`session_action`].
    SessionAction,
    /// The outcome of [`Cmd::AttachSession`] when it came from
    /// [`Msg::SessionAction`], as a ready-to-display status line.
    SessionAttachResult(String),
    /// [`Cmd::LoadAuditStatus`] finished loading, replacing
    /// `app.audit_status` wholesale.
    AuditStatusLoaded(HashMap<String, AuditStatusEntry>),
    /// The outcome of [`Cmd::LaunchAudit`] or [`Cmd::AttachSession`], already
    /// rendered to a human-readable status-line message (e.g. `launched
    /// audit for PROJ-1 -- press a to attach`, or `detached from
    /// tm-proj-proj-1`). Kept as a single string variant (mirroring
    /// [`Msg::RankApplied`]/[`Msg::TicketsFailed`]'s reuse pattern) rather
    /// than a dedicated success/failure pair, since every outcome is purely
    /// a status-line update from `update`'s point of view.
    AuditActionResult(String),
    /// The `w` key was pressed on [`Screen::Board`] for the selected ticket:
    /// launch a lane run for it, per [`lane_run_action`]'s precedence rule
    /// (already-active guard, zero/one/many configured lanes).
    LaneRunAction,
    /// Move the lane picker's highlighted lane up.
    LanePickerUp,
    /// Move the lane picker's highlighted lane down.
    LanePickerDown,
    /// Launch a lane run for the selected ticket using the picker's
    /// highlighted lane, and close the picker.
    LanePickerSelect,
    /// Close the lane picker without launching anything.
    LanePickerClose,
    /// The outcome of [`Cmd::LaunchLaneRun`] for `key`: removes it from
    /// `pending_lane_launches` and sets a human-readable status-line message
    /// either way.
    LaneRunLaunchResult {
        /// Ticket key the launch was for.
        key: String,
        /// `Ok(())` if the launcher child exited zero (the run row now
        /// exists; badge polling takes over), `Err` with a status-line
        /// message otherwise.
        result: Result<(), String>,
    },
    /// [`Cmd::LoadLaneRunStatus`] finished loading, replacing
    /// `app.lane_run_status` wholesale.
    LaneRunStatusLoaded(HashMap<String, RunIndicator>),
    /// [`Cmd::LoadBotWatchStatus`] finished loading, replacing
    /// `app.bot_watch_status` wholesale.
    BotWatchStatusLoaded(HashMap<String, BotWatchIndicator>),
    /// [`Cmd::LoadCleanupStatus`] finished loading, replacing
    /// `app.cleanup_status` wholesale.
    CleanupStatusLoaded(HashMap<String, AuditStatusEntry>),
    /// The `b` key was pressed on [`Screen::Board`] for the selected ticket:
    /// attach to its live cleanup session, launch one, or arm a PR bot
    /// watcher, per [`bots_action`]'s precedence rule.
    BotsAction,
    /// The outcome of [`Cmd::LaunchCleanup`] or of attaching to a cleanup
    /// session, already rendered to a human-readable status-line message.
    /// Kept as a single string variant for the same reason
    /// [`Msg::AuditActionResult`] is.
    BotsActionResult(String),
    /// The outcome of [`Cmd::LaunchBotWatch`] for `key`: removes it from
    /// `pending_bot_watch_launches` and sets a human-readable status-line
    /// message either way.
    BotWatchLaunchResult {
        /// Ticket key the watcher was armed for.
        key: String,
        /// `Ok(())` if `tm pr watch` exited zero (the watcher is detached and
        /// its run row now exists; badge polling takes over), `Err` with its
        /// stderr otherwise (e.g. no open PR, or already watching).
        result: Result<(), String>,
    },
    /// Open the retro board. Only meaningful on [`Screen::Board`];
    /// [`crate::tui::keymap::map_key`] only ever emits this from there.
    OpenRetro,
    /// The retro board's ticket list finished loading (already filtered
    /// against recorded retro verdicts, and enriched with run cost/model
    /// info -- see [`crate::tui::event::fetch_retro_tickets`]).
    RetroTicketsLoaded(Vec<RetroRow>),
    /// The retro board's ticket list failed to load.
    RetroTicketsFailed(String),
    /// The `d` key was pressed on [`Screen::Retro`] for the highlighted
    /// ticket: begin the defect flow by opening the severity picker. A
    /// no-op when nothing is highlighted.
    RetroDefectStart,
    /// Move the severity picker's highlighted option up.
    RetroSeverityPickerUp,
    /// Move the severity picker's highlighted option down.
    RetroSeverityPickerDown,
    /// Confirm the severity picker's highlighted option and move on to the
    /// (optional) note-entry step.
    RetroSeverityPickerSelect,
    /// Cancel the defect flow from the severity picker, discarding it
    /// entirely -- the ticket stays on the board, no verdict recorded.
    RetroSeverityPickerClose,
    /// Append `char` to the in-progress note.
    RetroNoteChar(char),
    /// Remove the last character of the in-progress note.
    RetroNoteBackspace,
    /// Submit the defect flow: record [`RetroVerdict::Defect`] with the
    /// chosen severity and whatever note text has been typed (empty ->
    /// `None`).
    RetroNoteSubmit,
    /// Cancel the defect flow from the note-entry step, discarding it
    /// entirely -- same effect as [`Msg::RetroSeverityPickerClose`], one
    /// step later.
    RetroNoteCancel,
    /// The `c` key was pressed on [`Screen::Retro`] for the highlighted
    /// ticket: record [`RetroVerdict::Clean`] directly, no picker. A no-op
    /// when nothing is highlighted.
    RetroMarkClean,
    /// [`Cmd::RecordRetro`] succeeded: drop `key` from `retro_tickets` (it
    /// now has a verdict) and report it in the status line.
    RetroRecorded {
        /// Ticket key the verdict was recorded for.
        key: String,
        /// The verdict that was recorded.
        verdict: RetroVerdict,
    },
    /// [`Cmd::RecordRetro`] failed: the ticket stays in `retro_tickets`
    /// (nothing was written), and the error is reported in the status line.
    RetroFailed(String),
}

/// I/O the caller should perform as a result of [`update`].
///
/// `update` itself never performs I/O; it only describes what should happen.
/// The caller (`crate::tui::event`) executes each `Cmd` and feeds the
/// resulting `Msg` back through `update`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    /// Fetch tickets matching `query`, built by [`query_for_filter`] from the
    /// board's active [`AssigneeFilter`].
    FetchTickets {
        /// The query to search with.
        query: TicketQuery,
    },
    /// Fetch assignable users for `project`, for the filter picker.
    FetchAssignableUsers {
        /// Project key to list assignable users for.
        project: String,
    },
    /// Change `key`'s assignee per `choice`. [`AssignChoice::Me`] is
    /// resolved in the executor via
    /// [`crate::ticketing::provider::TicketProvider::myself`], which is
    /// backend-correct under both Jira and GitHub and yields a display name
    /// for the card; the CLI's cached-accountId fast path is deliberately
    /// not used here.
    AssignTicket {
        /// Ticket key to (un)assign.
        key: String,
        /// Who to assign it to.
        choice: AssignChoice,
    },
    /// Fetch the workflow transitions available on `key`.
    FetchTransitions {
        /// Ticket key to fetch transitions for.
        key: String,
    },
    /// Apply `transition_id` to `key`.
    ApplyTransition {
        /// Ticket key to transition.
        key: String,
        /// ID of the transition to apply.
        transition_id: String,
    },
    /// Open `url` in the user's default browser.
    OpenUrl(String),
    /// Fetch every open ticket in the project, in Jira backlog rank order,
    /// for [`Screen::Rank`].
    FetchRankTickets {
        /// The query to search with, always [`TicketQuery::Ranked`].
        query: TicketQuery,
    },
    /// Re-rank `key` relative to `anchor`.
    RankTicket {
        /// Ticket key to move.
        key: String,
        /// Where to move it to.
        anchor: RankAnchor,
    },
    /// Reload [`Screen::Runs`]'s kanban board from the run store.
    LoadRuns,
    /// Load the full detail (including its event timeline) of one run, for
    /// the run detail floating window.
    LoadRunDetail {
        /// Row id of the run to load.
        run_id: i64,
    },
    /// Load the full detail of `key`'s latest run (any `kind`), for the run
    /// detail floating window opened from [`Screen::Board`] via
    /// [`Msg::ViewRunAction`]. Unlike [`Cmd::LoadRunDetail`], resolves by
    /// ticket rather than run id, so a refresh always picks up whichever run
    /// is now the latest -- see `docs/plans/board-run-detail.md`'s "Ticket-
    /// keyed load Cmd" decision.
    LoadTicketRunDetail {
        /// Ticket key to load the latest run for.
        key: String,
    },
    /// Reap abandoned runs in the run store.
    ReapRuns,
    /// Reload [`Screen::Board`]'s per-ticket audit status (live `audit`
    /// tmux windows plus the latest `kind = "audit"` run per ticket).
    LoadAuditStatus,
    /// Launch a ticket-audit session for `key` (see
    /// [`crate::work::audit::launch_audit`]).
    LaunchAudit {
        /// Ticket key to launch an audit session for.
        key: String,
    },
    /// Attach the terminal to `session_name` (the ticket's `tm-<scope>-<key>`
    /// session), suspending and restoring the board's terminal state around
    /// the blocking `tmux attach-session` call. Handled specially by the
    /// board's event loop, unlike every other `Cmd` here -- see
    /// `crate::tui::event`'s module docs.
    AttachSession {
        /// Name of the tmux session to attach to.
        session_name: String,
    },
    /// Launch a lane run for `key` on `lane` (see `tm work run <lane>
    /// <key>`, spawned via `std::env::current_exe()` as a watched child
    /// process -- see `docs/plans/board-lane-runs.md`'s "Launch mechanism").
    LaunchLaneRun {
        /// Configured lane name to run on.
        lane: String,
        /// Ticket key to launch a lane run for.
        key: String,
    },
    /// Reload [`Screen::Board`]'s per-ticket lane-run status (the latest
    /// `kind = "lane"` run per ticket, mapped through
    /// [`lane_run_indicator`]).
    LoadLaneRunStatus,
    /// Reload [`Screen::Board`]'s per-ticket PR bot-watch status (the latest
    /// `kind = "review-watch"` run per ticket, mapped through
    /// [`bot_watch_indicator`]).
    LoadBotWatchStatus,
    /// Reload [`Screen::Board`]'s per-ticket bugbot-cleanup session status
    /// (live `bugbot` tmux windows plus the latest
    /// `kind = "bugbot-cleanup"` run per ticket, mapped through the same
    /// [`audit_indicator`] rule audit sessions use).
    LoadCleanupStatus,
    /// Launch a bugbot-cleanup session for `key` (see
    /// [`crate::work::bugbot::launch_cleanup`]). Executed in-process, exactly
    /// like [`Cmd::LaunchAudit`]: the launch only pre-registers a run row and
    /// starts a tmux session, so it returns immediately.
    LaunchCleanup {
        /// Ticket key to launch a cleanup session for.
        key: String,
    },
    /// Arm a PR bot watcher for `key` (`tm pr watch <key>`, spawned via
    /// `std::env::current_exe()` as a watched child process through the same
    /// [`crate::tui::launcher::LaneLauncher`] seam [`Cmd::LaunchLaneRun`]
    /// uses -- see `docs/plans/bugbot-watch.md`'s "Board integration").
    LaunchBotWatch {
        /// Ticket key to arm a watcher for.
        key: String,
    },
    /// Open the selected ticket's latest run's log file in a pager,
    /// suspending and restoring the board's terminal state around the
    /// blocking call -- handled specially by the board's event loop, exactly
    /// like [`Cmd::AttachSession`].
    ViewLogs {
        /// Ticket key to view the latest run's log for.
        key: String,
    },
    /// Open `key`'s worktree (its latest `kind = "lane"` run's `worktree`)
    /// in `vdiff`, suspending and restoring the board's terminal state
    /// around the blocking call -- handled specially by the board's event
    /// loop, exactly like [`Cmd::ViewLogs`] and [`Cmd::AttachSession`], since
    /// `vdiff` is an interactive GUI/TUI that needs the real TTY (see
    /// `docs/plans/board-vdiff-review-loop.md`'s "Decisions" section for why
    /// this is foreground/suspending rather than the watched-child seam
    /// [`Cmd::LaunchLaneRun`]/[`Cmd::LaunchBotWatch`]/
    /// [`Cmd::LaunchReviewFix`] use).
    ViewDiff {
        /// Ticket key to open the latest lane run's worktree for.
        key: String,
    },
    /// Dispatch a fix pass over `key`'s captured `vdiff` review comments
    /// (`tm review fix <key>`, spawned via `std::env::current_exe()` as a
    /// watched child process through the same
    /// [`crate::tui::launcher::LaneLauncher`] seam [`Cmd::LaunchLaneRun`]/
    /// [`Cmd::LaunchBotWatch`] use -- see
    /// `docs/plans/board-vdiff-review-loop.md`'s "Decisions" section).
    LaunchReviewFix {
        /// Ticket key to dispatch a fix pass for.
        key: String,
    },
    /// Resolve whether `key` has an open GitHub pull request, for
    /// [`Msg::OpenBrowserAction`]'s picker-or-direct-open decision. Reports
    /// back as [`Msg::BrowserOptionsResolved`]. One `gh` call, made only on
    /// this keypress -- never prefetched for every card on refresh.
    ResolvePrForTicket {
        /// Ticket key to resolve a PR for.
        key: String,
        /// The ticket's Jira URL, threaded through to
        /// [`Msg::BrowserOptionsResolved`] so it can open Jira directly when
        /// no PR is found without a second lookup.
        jira_url: String,
    },
    /// Fetch shipped tickets matching `query` (always
    /// [`TicketQuery::ShippedAwaitingRetro`]), filter out any that already
    /// have a recorded retro verdict, and enrich the rest with their latest
    /// `kind = "lane"` run's cost/model info, for [`Screen::Retro`].
    FetchRetroTickets {
        /// The query to search with.
        query: TicketQuery,
    },
    /// Record a retro verdict for `key`.
    RecordRetro {
        /// Ticket key to record a verdict for.
        key: String,
        /// The verdict.
        verdict: RetroVerdict,
        /// Defect severity; must be `Some` for
        /// [`RetroVerdict::Defect`] and `None` for [`RetroVerdict::Clean`]
        /// (enforced by [`crate::runs::RunStore::record_retro`]).
        severity: Option<RetroSeverity>,
        /// Optional free-text note.
        notes: Option<String>,
    },
}

/// Advance `app` in response to `msg`, returning the new state and any
/// commands the caller should execute.
///
/// Pure: performs no I/O. All failure-mode messages (`*Failed`) set
/// `status_line` rather than panicking.
pub fn update(mut app: App, msg: Msg) -> (App, Vec<Cmd>) {
    match msg {
        Msg::Up => {
            move_up(&mut app);
            (app, Vec::new())
        }
        Msg::Down => {
            move_down(&mut app);
            (app, Vec::new())
        }
        Msg::Left => {
            move_left(&mut app);
            (app, Vec::new())
        }
        Msg::Right => {
            move_right(&mut app);
            (app, Vec::new())
        }
        Msg::Refresh => {
            app.status_line = "Refreshing...".to_string();
            if app.screen == Screen::Runs {
                let mut cmds = vec![Cmd::LoadRuns];
                if app.show_run_detail
                    && let Some(card) = app.selected_run_card()
                {
                    cmds.push(Cmd::LoadRunDetail { run_id: card.id });
                }
                (app, cmds)
            } else if app.screen == Screen::Rank {
                let query = TicketQuery::Ranked {
                    project_key: app.project_key.clone(),
                };
                (app, vec![Cmd::FetchRankTickets { query }])
            } else {
                let query = query_for_filter(&app.filter, &app.project_key);
                (app, vec![Cmd::FetchTickets { query }])
            }
        }
        Msg::OpenInBrowser => {
            let ticket = if app.screen == Screen::Rank {
                app.rank_selected_ticket()
            } else {
                app.selected_ticket()
            };
            let cmds = match ticket {
                Some(ticket) => vec![Cmd::OpenUrl(ticket.url.clone())],
                None => Vec::new(),
            };
            (app, cmds)
        }
        Msg::OpenBrowserAction => open_browser_action(app),
        Msg::BrowserOptionsResolved {
            key,
            jira_url,
            pr,
            note,
        } => browser_options_resolved(app, key, jira_url, pr, note),
        Msg::BrowserPickerUp => {
            app.browser_picker_selected = app.browser_picker_selected.saturating_sub(1);
            (app, Vec::new())
        }
        Msg::BrowserPickerDown => {
            let count = app.browser_picker_options.len();
            if count > 0 {
                app.browser_picker_selected = (app.browser_picker_selected + 1).min(count - 1);
            }
            (app, Vec::new())
        }
        Msg::BrowserPickerSelect => browser_picker_select(app),
        Msg::BrowserPickerClose => {
            app.show_browser_picker = false;
            (app, Vec::new())
        }
        Msg::ToggleHelp => {
            app.show_help = !app.show_help;
            (app, Vec::new())
        }
        Msg::Quit => {
            app.quit = true;
            (app, Vec::new())
        }
        Msg::TicketsLoaded(tickets) => {
            let preferred_key = app.selected_ticket().map(|t| t.key.clone());
            app.columns = group_into_columns(tickets, &app.board_column_order);
            reselect(&mut app, preferred_key);
            (app, Vec::new())
        }
        Msg::TicketsFailed(err) => {
            app.status_line = err;
            (app, Vec::new())
        }
        Msg::SearchTruncated { shown } => {
            app.status_line =
                format!("showing first {shown} tickets -- more matched; narrow the filter");
            (app, Vec::new())
        }
        Msg::Enter => enter(app),
        Msg::Back => {
            back(&mut app);
            (app, Vec::new())
        }
        Msg::TransitionsLoaded(transitions) => {
            app.transitions = transitions;
            app.transition_selected = 0;
            app.screen = Screen::TransitionMenu;
            (app, Vec::new())
        }
        Msg::TransitionsFailed(err) => {
            app.status_line = err;
            (app, Vec::new())
        }
        Msg::TransitionApplied {
            key,
            status,
            status_category,
        } => {
            let mut tickets = flatten(&app.columns);
            if let Some(ticket) = tickets.iter_mut().find(|t| t.key == key) {
                ticket.status = status.clone();
                ticket.status_category = status_category;
            }
            app.columns = group_into_columns(tickets, &app.board_column_order);
            reselect(&mut app, Some(key.clone()));
            app.status_line = format!("{key} -> {status}");
            app.screen = Screen::Detail;
            (app, Vec::new())
        }
        Msg::TransitionFailed(err) => {
            app.status_line = err;
            (app, Vec::new())
        }
        Msg::OpenFilterPicker => open_filter_picker(app),
        Msg::FilterPickerUp => {
            app.filter_picker_selected = app.filter_picker_selected.saturating_sub(1);
            (app, Vec::new())
        }
        Msg::FilterPickerDown => {
            let count = app.filter_options().len();
            if count > 0 {
                app.filter_picker_selected = (app.filter_picker_selected + 1).min(count - 1);
            }
            (app, Vec::new())
        }
        Msg::FilterPickerSelect => filter_picker_select(app),
        Msg::FilterPickerClose => {
            app.show_filter_picker = false;
            (app, Vec::new())
        }
        Msg::AssignableUsersLoaded(users) => {
            app.assignable_users = Some(users);
            app.filter_picker_error = None;
            app.assign_picker_error = None;
            let count = app.filter_options().len();
            if count > 0 {
                app.filter_picker_selected = app.filter_picker_selected.min(count - 1);
            }
            let count = app.assign_options().len();
            if count > 0 {
                app.assign_picker_selected = app.assign_picker_selected.min(count - 1);
            }
            (app, Vec::new())
        }
        Msg::AssignableUsersFailed(err) => {
            app.filter_picker_error = Some(err.clone());
            app.assign_picker_error = Some(err);
            (app, Vec::new())
        }
        Msg::OpenAssignPicker => open_assign_picker(app),
        Msg::AssignPickerUp => {
            app.assign_picker_selected = app.assign_picker_selected.saturating_sub(1);
            (app, Vec::new())
        }
        Msg::AssignPickerDown => {
            let count = app.assign_options().len();
            if count > 0 {
                app.assign_picker_selected = (app.assign_picker_selected + 1).min(count - 1);
            }
            (app, Vec::new())
        }
        Msg::AssignPickerSelect => assign_picker_select(app),
        Msg::AssignPickerClose => {
            app.show_assign_picker = false;
            (app, Vec::new())
        }
        Msg::AssignApplied { key, assignee } => {
            for column in &mut app.columns {
                for ticket in &mut column.tickets {
                    if ticket.key == key {
                        ticket.assignee = assignee.clone();
                    }
                }
            }
            app.status_line = match &assignee {
                Some(name) => format!("{key} -> assigned to {name}"),
                None => format!("{key} -> unassigned"),
            };
            // The board's assignee filter (`f`) is applied server-side, so an
            // assign/unassign can move the card out from under the active
            // filter (e.g. assigning away from yourself under the default
            // `Me` filter). Refetch under the current filter so a
            // now-non-matching card doesn't linger until a manual `r`; the
            // local patch above still keeps the card visually correct in the
            // meantime.
            let query = query_for_filter(&app.filter, &app.project_key);
            (app, vec![Cmd::FetchTickets { query }])
        }
        Msg::AssignFailed(err) => {
            app.status_line = err;
            (app, Vec::new())
        }
        Msg::OpenRank => open_rank(app),
        Msg::RankTicketsLoaded(tickets) => {
            rank_tickets_loaded(&mut app, tickets);
            (app, Vec::new())
        }
        Msg::RankTicketsFailed(err) => {
            app.status_line = err;
            (app, Vec::new())
        }
        Msg::RankGrabToggle => rank_grab_toggle(app),
        Msg::RankApplied(message) => {
            app.status_line = message;
            (app, Vec::new())
        }
        Msg::RankFailed(err) => {
            app.status_line = err;
            let query = TicketQuery::Ranked {
                project_key: app.project_key.clone(),
            };
            (app, vec![Cmd::FetchRankTickets { query }])
        }
        Msg::Tick => tick(app),
        Msg::RunsLoaded(cards) => {
            runs_loaded(&mut app, cards);
            (app, Vec::new())
        }
        Msg::RunsFailed(err) => {
            app.status_line = err;
            (app, Vec::new())
        }
        Msg::RunDetailLoaded(detail) => {
            app.run_detail = Some(*detail);
            (app, Vec::new())
        }
        Msg::RunDetailFailed(err) => {
            app.status_line = err;
            // A failed refresh after a successful load leaves the loaded
            // content up; a failure before anything ever loaded closes the
            // overlay rather than leaving the user stuck on "Loading...".
            if app.run_detail.is_none() {
                app.show_run_detail = false;
            }
            (app, Vec::new())
        }
        Msg::ViewRunAction => view_run_action(app),
        Msg::ViewLogsAction => view_logs_action(app),
        Msg::LogsActionResult(message) => {
            app.status_line = message;
            (app, Vec::new())
        }
        Msg::ViewDiffAction => view_diff_action(app),
        Msg::DiffActionResult(message) => {
            app.status_line = message;
            (app, Vec::new())
        }
        Msg::ReviewFixAction => review_fix_action(app),
        Msg::ReviewFixLaunchResult { key, result } => {
            app.status_line = match result {
                Ok(()) => format!("fix pass dispatched for {key}"),
                Err(err) => format!("fix pass for {key} failed: {err}"),
            };
            (app, Vec::new())
        }
        Msg::RunsReaped(count) => {
            if count > 0 {
                app.status_line = format!("Reaped {count} dead run(s)");
            }
            (app, Vec::new())
        }
        Msg::AuditAction => audit_action(app),
        Msg::SessionAction => session_action(app),
        Msg::SessionAttachResult(message) => {
            app.status_line = message;
            (app, Vec::new())
        }
        Msg::AuditStatusLoaded(status) => {
            app.audit_status = status;
            (app, Vec::new())
        }
        Msg::AuditActionResult(message) => {
            app.status_line = message;
            (app, Vec::new())
        }
        Msg::LaneRunAction => lane_run_action(app),
        Msg::LanePickerUp => {
            app.lane_picker_selected = app.lane_picker_selected.saturating_sub(1);
            (app, Vec::new())
        }
        Msg::LanePickerDown => {
            let count = app.lane_names.len();
            if count > 0 {
                app.lane_picker_selected = (app.lane_picker_selected + 1).min(count - 1);
            }
            (app, Vec::new())
        }
        Msg::LanePickerSelect => lane_picker_select(app),
        Msg::LanePickerClose => {
            app.show_lane_picker = false;
            (app, Vec::new())
        }
        Msg::LaneRunLaunchResult { key, result } => {
            app.pending_lane_launches.remove(&key);
            app.status_line = match result {
                Ok(()) => format!("launched lane run for {key}"),
                Err(err) => format!("lane run launch failed for {key}: {err}"),
            };
            (app, Vec::new())
        }
        Msg::LaneRunStatusLoaded(mut status) => {
            // The executor's `load_lane_run_status` has no access to
            // `pending_lane_launches` (see its doc comment), so a ticket
            // whose launcher child is still in flight but has no run row
            // yet would otherwise drop out of `lane_run_status` -- and thus
            // its badge, which `ui.rs` reads only from this map -- on every
            // refresh. Overlay `Starting` for any such ticket here;
            // `or_insert` leaves an already-loaded run row's real status
            // untouched.
            for key in &app.pending_lane_launches {
                status.entry(key.clone()).or_insert(RunIndicator::Starting);
            }
            app.lane_run_status = status;
            (app, Vec::new())
        }
        Msg::BotWatchStatusLoaded(status) => {
            app.bot_watch_status = status;
            (app, Vec::new())
        }
        Msg::CleanupStatusLoaded(status) => {
            app.cleanup_status = status;
            (app, Vec::new())
        }
        Msg::BotsAction => bots_action(app),
        Msg::BotsActionResult(message) => {
            app.status_line = message;
            (app, Vec::new())
        }
        Msg::BotWatchLaunchResult { key, result } => {
            app.pending_bot_watch_launches.remove(&key);
            app.status_line = match result {
                Ok(()) => format!("watching PR for {key}"),
                Err(err) => format!("PR watch failed for {key}: {err}"),
            };
            (app, Vec::new())
        }
        Msg::OpenRetro => open_retro(app),
        Msg::RetroTicketsLoaded(tickets) => {
            retro_tickets_loaded(&mut app, tickets);
            (app, Vec::new())
        }
        Msg::RetroTicketsFailed(err) => {
            app.status_line = err;
            (app, Vec::new())
        }
        Msg::RetroDefectStart => retro_defect_start(app),
        Msg::RetroSeverityPickerUp => {
            app.retro_severity_selected = app.retro_severity_selected.saturating_sub(1);
            (app, Vec::new())
        }
        Msg::RetroSeverityPickerDown => {
            app.retro_severity_selected =
                (app.retro_severity_selected + 1).min(RETRO_SEVERITIES.len() - 1);
            (app, Vec::new())
        }
        Msg::RetroSeverityPickerSelect => retro_severity_picker_select(app),
        Msg::RetroSeverityPickerClose => {
            retro_cancel_defect_flow(&mut app);
            (app, Vec::new())
        }
        Msg::RetroNoteChar(c) => {
            app.retro_note_draft.push(c);
            (app, Vec::new())
        }
        Msg::RetroNoteBackspace => {
            app.retro_note_draft.pop();
            (app, Vec::new())
        }
        Msg::RetroNoteSubmit => retro_note_submit(app),
        Msg::RetroNoteCancel => {
            retro_cancel_defect_flow(&mut app);
            (app, Vec::new())
        }
        Msg::RetroMarkClean => retro_mark_clean(app),
        Msg::RetroRecorded { key, verdict } => {
            retro_recorded(&mut app, &key, verdict);
            (app, Vec::new())
        }
        Msg::RetroFailed(err) => {
            app.status_line = err;
            (app, Vec::new())
        }
    }
}

/// Handle [`Msg::OpenBrowserAction`]: the `o` key's browser-open entry point
/// on [`Screen::Board`], per the feature's "Target behavior" decisions. A
/// no-op when no ticket is selected, mirroring [`Msg::OpenInBrowser`]. Only
/// meaningful on [`Screen::Board`] -- [`crate::tui::keymap::map_key`] only
/// ever emits this from there, but (like [`view_run_action`]) the check is
/// kept explicit rather than relying solely on that gating.
///
/// Never opens anything directly: it always defers to
/// [`Cmd::ResolvePrForTicket`], whose result ([`Msg::BrowserOptionsResolved`])
/// is what actually decides between the picker and a direct Jira open. This
/// keeps the PR lookup a single `gh` call made only on this keypress, per the
/// feature's "on keypress only" requirement -- no board refresh ever
/// prefetches PR data for every card.
///
/// Sets `status_line` to `resolving PR for <key>...` before returning the
/// `Cmd` -- the lookup is a blocking `gh pr list` call (bounded, but still
/// synchronous inside the event loop; see
/// [`crate::tui::event::resolve_pr_for_ticket`]'s doc comment), and
/// `crate::tui::event::run_cmds` forces a redraw with this message on screen
/// before running it, so the board never just looks hung while it waits.
fn open_browser_action(mut app: App) -> (App, Vec<Cmd>) {
    if app.screen != Screen::Board {
        return (app, Vec::new());
    }
    let Some(ticket) = app.selected_ticket() else {
        return (app, Vec::new());
    };
    let key = ticket.key.clone();
    let jira_url = ticket.url.clone();
    app.status_line = format!("resolving PR for {key}...");
    (app, vec![Cmd::ResolvePrForTicket { key, jira_url }])
}

/// Handle [`Msg::BrowserOptionsResolved`]: [`Cmd::ResolvePrForTicket`]'s
/// result decides between showing the browser picker and opening Jira
/// directly, mirroring [`lane_run_action`]'s zero/one/many short-circuit
/// precedent (see `src/tui/app.rs:1298`, "Key existing code" in the feature
/// brief) -- here the split is binary: found a PR, or didn't.
///
/// - `pr` is `Some` -> build the picker's two options (Jira first, then
///   GitHub) and show it, highlighting the first entry.
/// - `pr` is `None` -> skip the picker entirely and open `jira_url` directly,
///   the same "ticket not yet in code review, or no PR resolves to it"
///   fallback the feature brief specifies. This also covers `gh` being
///   unavailable, the repo not resolving, or the lookup erroring --
///   [`crate::tui::event::TuiDeps`]'s "a broken dependency degrades one
///   feature, it must never block the Jira board" stance applies here too, so
///   every one of those cases collapses to the same "no PR" case rather than
///   surfacing an error.
///
/// `note`, when `Some`, overwrites `status_line` before the picker/Jira
/// decision -- currently only set when the lookup timed out (see
/// [`crate::tui::event::resolve_pr_for_ticket`]), so the user landing on Jira
/// unexpectedly (rather than the picker) has a visible reason why, instead of
/// it looking like the ticket simply has no PR.
fn browser_options_resolved(
    mut app: App,
    key: String,
    jira_url: String,
    pr: Option<crate::github::pr::PrInfo>,
    note: Option<String>,
) -> (App, Vec<Cmd>) {
    if let Some(note) = note {
        app.status_line = note;
    }
    match pr {
        Some(pr) => {
            app.browser_picker_options = vec![
                BrowserPickerOption::Jira { key, url: jira_url },
                BrowserPickerOption::GitHub {
                    number: pr.number,
                    url: pr.url,
                },
            ];
            app.browser_picker_selected = 0;
            app.show_browser_picker = true;
            (app, Vec::new())
        }
        None => (app, vec![Cmd::OpenUrl(jira_url)]),
    }
}

/// Handle [`Msg::BrowserPickerSelect`]: open the browser picker's highlighted
/// option's URL, and close the picker.
///
/// A no-op (picker stays open) when `browser_picker_selected` is out of
/// range, mirroring [`filter_picker_select`]/[`lane_picker_select`]'s
/// out-of-range behavior -- which in practice only happens if the option list
/// were ever empty, since [`browser_options_resolved`] always seeds exactly
/// two options before showing the picker.
fn browser_picker_select(mut app: App) -> (App, Vec<Cmd>) {
    let Some(option) = app.browser_picker_options.get(app.browser_picker_selected) else {
        return (app, Vec::new());
    };
    let url = option.url().to_string();
    app.show_browser_picker = false;
    (app, vec![Cmd::OpenUrl(url)])
}

/// Handle [`Msg::BotsAction`]: the `b` key's attach-or-launch-or-arm
/// precedence for the selected board ticket, per
/// `docs/plans/bugbot-watch.md`'s "Board integration" section. Mirrors
/// [`audit_action`]'s shape, with two more steps in front of the launch.
///
/// A no-op when no ticket is selected. Otherwise, in order:
///
/// 1. A live `bugbot` window exists -> attach to the ticket's session
///    ([`Cmd::AttachSession`], which takes a session name rather than anything
///    audit-specific).
/// 2. No live window but the latest watcher run is
///    [`BotWatchIndicator::Ready`] (the bots reviewed and left unresolved
///    findings) -> [`Cmd::LaunchCleanup`].
/// 3. The latest watcher run is [`BotWatchIndicator::Watching`], or a
///    watcher launch is still in flight -> a status-line message only; there
///    is nothing to act on until the bots finish.
/// 4. Otherwise (no watcher, or the last one is `Clean`/`Failed`) -> arm a new
///    one ([`Cmd::LaunchBotWatch`]), marking the ticket pending so the badge
///    reads as starting until the launcher child reports back.
fn bots_action(mut app: App) -> (App, Vec<Cmd>) {
    let Some(ticket) = app.selected_ticket() else {
        return (app, Vec::new());
    };
    let key = ticket.key.clone();

    let window_live = app
        .cleanup_status
        .get(&key)
        .is_some_and(|entry| entry.window_live);
    if window_live {
        let session_name = crate::work::naming::ticket_session_name(&app.session_slug, &key);
        return (app, vec![Cmd::AttachSession { session_name }]);
    }

    match app.bot_watch_status.get(&key) {
        Some(BotWatchIndicator::Ready) => (app, vec![Cmd::LaunchCleanup { key }]),
        Some(BotWatchIndicator::Watching) => {
            app.status_line = format!("watching PR for {key} -- bots not done yet");
            (app, Vec::new())
        }
        _ if app.pending_bot_watch_launches.contains(&key) => {
            app.status_line = format!("arming PR watcher for {key}");
            (app, Vec::new())
        }
        _ => {
            app.pending_bot_watch_launches.insert(key.clone());
            (app, vec![Cmd::LaunchBotWatch { key }])
        }
    }
}

/// Handle [`Msg::LaneRunAction`]: launch a lane run for the selected board
/// ticket, per `docs/plans/board-lane-runs.md`'s "Decisions" section.
///
/// A no-op when no ticket is selected. If the ticket already has an active
/// lane run (pending launch, or [`RunIndicator::Starting`]/`Running`/
/// `Waiting` in `lane_run_status`), sets a status message instead of
/// launching another one -- terminal indicators (`Done`/`Failed`) don't
/// block a relaunch. Otherwise: zero configured (repo-compatible) lanes sets
/// a status message -- noting how many lanes were hidden for a backend
/// mismatch (`hidden_lane_count`) when that's why the list is empty, rather
/// than claiming nothing is configured at all; exactly one lane launches
/// directly (marking the ticket pending); more than one opens the lane
/// picker, highlighting the first lane.
fn lane_run_action(mut app: App) -> (App, Vec<Cmd>) {
    let Some(ticket) = app.selected_ticket() else {
        return (app, Vec::new());
    };
    let key = ticket.key.clone();

    let active = app.pending_lane_launches.contains(&key)
        || matches!(
            app.lane_run_status.get(&key),
            Some(RunIndicator::Starting | RunIndicator::Running | RunIndicator::Waiting)
        );
    if active {
        app.status_line = format!("lane run already active for {key}");
        return (app, Vec::new());
    }

    match app.lane_names.len() {
        0 => {
            app.status_line = if app.hidden_lane_count > 0 {
                format!(
                    "no compatible lanes ({} hidden: backend mismatch)",
                    app.hidden_lane_count
                )
            } else {
                "no lanes configured".to_string()
            };
            (app, Vec::new())
        }
        1 => {
            let lane = app.lane_names[0].clone();
            app.pending_lane_launches.insert(key.clone());
            (app, vec![Cmd::LaunchLaneRun { lane, key }])
        }
        _ => {
            app.show_lane_picker = true;
            app.lane_picker_selected = 0;
            (app, Vec::new())
        }
    }
}

/// Handle [`Msg::LanePickerSelect`]: launch a lane run for the selected board
/// ticket using the picker's highlighted lane, and close the picker.
///
/// A no-op (picker stays open) when `lane_picker_selected` is out of range,
/// mirroring [`filter_picker_select`]'s out-of-range behavior. If somehow no
/// ticket is selected, closes the picker without launching anything.
fn lane_picker_select(mut app: App) -> (App, Vec<Cmd>) {
    let Some(lane) = app.lane_names.get(app.lane_picker_selected).cloned() else {
        return (app, Vec::new());
    };
    let Some(ticket) = app.selected_ticket() else {
        app.show_lane_picker = false;
        return (app, Vec::new());
    };
    let key = ticket.key.clone();
    app.show_lane_picker = false;
    app.pending_lane_launches.insert(key.clone());
    (app, vec![Cmd::LaunchLaneRun { lane, key }])
}

/// Handle [`Msg::AuditAction`]: for the selected board ticket, attach to its
/// `tm-<scope>-<key>` session if [`App::audit_status`] reports a live `audit`
/// window, otherwise launch a new one. A no-op when no ticket is selected.
fn audit_action(app: App) -> (App, Vec<Cmd>) {
    let Some(ticket) = app.selected_ticket() else {
        return (app, Vec::new());
    };
    let key = ticket.key.clone();
    let window_live = app
        .audit_status
        .get(&key)
        .is_some_and(|entry| entry.window_live);
    let cmd = if window_live {
        Cmd::AttachSession {
            session_name: crate::work::naming::ticket_session_name(&app.session_slug, &key),
        }
    } else {
        Cmd::LaunchAudit { key }
    };
    (app, vec![cmd])
}

/// Handle [`Msg::SessionAction`]: attach to the selected board ticket's
/// `tm-<scope>-<key>` session. A no-op when no ticket is selected.
///
/// Deliberately unconditional, unlike [`audit_action`]'s attach-or-launch.
/// Two reasons:
///
/// - **There is nothing to launch.** A ticket's session is created by
///   whichever action happens to run first; `s` means "show me this ticket's
///   session", not "start something". Launching an action as a side effect of
///   asking to look would be a surprise.
/// - **No new liveness map is needed.** `audit_status`/`cleanup_status`
///   report whether a *specific action's window* is live, which is not the
///   same question as "does the session exist" — a session holding only a
///   `work` window, or only a `shell`, is perfectly attachable. Answering the
///   session question properly would mean another polled map on the board;
///   instead `tmux attach-session` answers it, and its failure becomes the
///   status line (see [`crate::tui::event`]'s `attach_session`). Cheaper, and
///   it cannot go stale between poll and keypress.
fn session_action(app: App) -> (App, Vec<Cmd>) {
    let Some(ticket) = app.selected_ticket() else {
        return (app, Vec::new());
    };
    let session_name = crate::work::naming::ticket_session_name(&app.session_slug, &ticket.key);
    (app, vec![Cmd::AttachSession { session_name }])
}

/// Handle [`Msg::ViewRunAction`]: open the run detail overlay on
/// [`Screen::Board`] for the selected ticket's latest run, per
/// `docs/plans/board-run-detail.md`. A no-op when no ticket is selected or
/// off [`Screen::Board`] -- [`crate::tui::keymap::map_key`] only ever emits
/// this from there, but the check is kept explicit (rather than relying
/// solely on that gating, unlike [`audit_action`]/[`lane_run_action`]) so
/// this stays independently testable.
fn view_run_action(mut app: App) -> (App, Vec<Cmd>) {
    if app.screen != Screen::Board {
        return (app, Vec::new());
    }
    let Some(ticket) = app.selected_ticket() else {
        return (app, Vec::new());
    };
    let key = ticket.key.clone();
    app.show_run_detail = true;
    app.run_detail = None;
    app.run_detail_scroll = 0;
    (app, vec![Cmd::LoadTicketRunDetail { key }])
}

/// Handle [`Msg::ViewLogsAction`]: open the selected ticket's latest run's
/// log file in a pager. A no-op (off [`Screen::Board`], or with no ticket
/// selected) mirroring [`view_run_action`]'s guard shape; actually resolving
/// a run/log path and shelling out happens in
/// [`crate::tui::event::run_cmds`]'s [`Cmd::ViewLogs`] interception, since
/// that needs the run store and a real terminal, neither of which `update`
/// has access to (it stays pure, per this module's doc comment).
fn view_logs_action(app: App) -> (App, Vec<Cmd>) {
    if app.screen != Screen::Board {
        return (app, Vec::new());
    }
    let Some(key) = app.selected_ticket().map(|ticket| ticket.key.clone()) else {
        return (app, Vec::new());
    };
    (app, vec![Cmd::ViewLogs { key }])
}

/// Handle [`Msg::ViewDiffAction`]: open the selected board ticket's worktree
/// in `vdiff`, per `docs/plans/board-vdiff-review-loop.md`'s "Decisions"
/// section.
///
/// A no-op (off [`Screen::Board`], or with no ticket selected) mirroring
/// [`view_logs_action`]'s guard shape. Gating is otherwise state-driven, not
/// column/status-driven (matching `a`/`w`, which have no per-status gating
/// either): a ticket with no entry in `lane_run_status` has no lane run at
/// all -- [`lane_run_indicator`] maps every possible run row to `Some`, so a
/// missing entry can only mean "no run row exists" -- and thus no worktree
/// for `vdiff` to open, so this sets a status-line message instead of
/// emitting [`Cmd::ViewDiff`]. A ticket whose lane run is still
/// [`RunIndicator::Starting`] (launcher child in flight, no run row yet)
/// gets the same treatment: there is no worktree path to resolve until the
/// run row exists. Every other indicator has a real run row -- and thus a
/// `worktree` column to resolve -- so [`Cmd::ViewDiff`]'s own resolution
/// (see [`crate::tui::event::resolve_vdiff_worktree`]) is left to catch the
/// rarer case of a worktree that was since removed (`tm work remove`).
fn view_diff_action(mut app: App) -> (App, Vec<Cmd>) {
    if app.screen != Screen::Board {
        return (app, Vec::new());
    }
    let Some(ticket) = app.selected_ticket() else {
        return (app, Vec::new());
    };
    let key = ticket.key.clone();
    match app.lane_run_status.get(&key) {
        None => {
            app.status_line = format!("no lane run for {key} -- press w first");
            (app, Vec::new())
        }
        Some(RunIndicator::Starting) => {
            app.status_line = format!("lane run for {key} is still starting");
            (app, Vec::new())
        }
        Some(_) => (app, vec![Cmd::ViewDiff { key }]),
    }
}

/// Handle [`Msg::ReviewFixAction`]: dispatch a fix pass over `key`'s
/// captured `vdiff` review comments, per
/// `docs/plans/board-vdiff-review-loop.md`'s "Decisions" section.
///
/// A no-op (off [`Screen::Board`], or with no ticket selected), and the same
/// "no lane run at all" gate [`view_diff_action`] uses -- for the identical
/// reason: `tm review fix` needs the ticket's existing worktree and branch,
/// which only exist once a lane run has provisioned them. Unlike
/// `view_diff_action`, a `RunIndicator::Starting` ticket is *not* blocked
/// here: the fix pass is a watched-child launch (like `w`/`b`), so it is
/// fine to queue it up even before the lane run's own preflight finishes --
/// `tm review fix`'s own preflight (see the module doc comment on
/// `docs/plans/board-vdiff-review-loop.md`) will simply fail fast with a
/// clear stderr message if the worktree still doesn't exist by the time it
/// runs, which [`Msg::ReviewFixLaunchResult`] surfaces in the status line.
fn review_fix_action(mut app: App) -> (App, Vec<Cmd>) {
    if app.screen != Screen::Board {
        return (app, Vec::new());
    }
    let Some(ticket) = app.selected_ticket() else {
        return (app, Vec::new());
    };
    let key = ticket.key.clone();
    if !app.lane_run_status.contains_key(&key) {
        app.status_line = format!("no lane run for {key} -- press w first");
        return (app, Vec::new());
    }
    app.status_line = format!("dispatching fix pass for {key}");
    (app, vec![Cmd::LaunchReviewFix { key }])
}

/// Handle [`Msg::Tick`]: a no-op off [`Screen::Runs`]/[`Screen::Board`].
///
/// On [`Screen::Runs`], increments `watch_tick` and emits [`Cmd::LoadRuns`]
/// every 2nd tick (plus [`Cmd::LoadRunDetail`] when the detail window is
/// open) and [`Cmd::ReapRuns`] every 120th tick (~30s at the 250ms poll
/// interval).
///
/// On [`Screen::Board`], increments the same counter and emits
/// [`Cmd::LoadAuditStatus`], [`Cmd::LoadLaneRunStatus`],
/// [`Cmd::LoadBotWatchStatus`] and [`Cmd::LoadCleanupStatus`] every 8th tick
/// (~2s) -- the board polls every badge source on its own clock while leaving
/// the Jira ticket list itself on manual refresh (`r`). The four polls stay
/// separate `Cmd`s (not merged into one query) since a ticket can legitimately
/// carry all four badges at once, each keyed off a different run `kind`.
/// Additionally, while the run detail overlay is open (`show_run_detail`),
/// emits [`Cmd::LoadTicketRunDetail`] for the selected ticket every 2nd tick
/// (~500ms), matching the watch screen's detail refresh cadence.
fn tick(mut app: App) -> (App, Vec<Cmd>) {
    match app.screen {
        Screen::Runs => {
            app.watch_tick += 1;
            let mut cmds = Vec::new();

            if app.watch_tick.is_multiple_of(2) {
                cmds.push(Cmd::LoadRuns);
                if app.show_run_detail
                    && let Some(card) = app.selected_run_card()
                {
                    cmds.push(Cmd::LoadRunDetail { run_id: card.id });
                }
            }

            if app.watch_tick.is_multiple_of(120) {
                cmds.push(Cmd::ReapRuns);
            }

            (app, cmds)
        }
        Screen::Board => {
            app.watch_tick += 1;
            let mut cmds = Vec::new();
            if app.watch_tick.is_multiple_of(8) {
                cmds.push(Cmd::LoadAuditStatus);
                cmds.push(Cmd::LoadLaneRunStatus);
                cmds.push(Cmd::LoadBotWatchStatus);
                cmds.push(Cmd::LoadCleanupStatus);
            }
            if app.watch_tick.is_multiple_of(2)
                && app.show_run_detail
                && let Some(ticket) = app.selected_ticket()
            {
                cmds.push(Cmd::LoadTicketRunDetail {
                    key: ticket.key.clone(),
                });
            }
            (app, cmds)
        }
        _ => (app, Vec::new()),
    }
}

/// Handle [`Msg::RunsLoaded`]: replace `app.runs` with server truth,
/// preferring to keep the previously selected run card selected (by id) if
/// it still exists, otherwise clamping the row within the current column
/// (mirroring [`clamp_row`]'s board behavior).
fn runs_loaded(app: &mut App, cards: Vec<RunCard>) {
    let preferred_id = app.selected_run_card().map(|c| c.id);
    app.runs = cards;

    let found = preferred_id.is_some_and(|id| select_run_by_id(app, id));
    if !found {
        clamp_runs_row(app);
    }
}

/// Select the run card with id `id`, if it exists in `app.runs`. Returns
/// whether it was found.
fn select_run_by_id(app: &mut App, id: i64) -> bool {
    for col in 0..RUN_COLUMNS.len() {
        if let Some(row) = app.runs_in_col(col).iter().position(|c| c.id == id) {
            app.runs_selected_col = col;
            app.runs_selected_row = row;
            return true;
        }
    }
    false
}

/// Clamp `runs_selected_row` into the bounds of the currently selected run
/// column, resetting to `0` when that column is empty.
fn clamp_runs_row(app: &mut App) {
    match app.runs_in_col(app.runs_selected_col).len() {
        0 => app.runs_selected_row = 0,
        len if app.runs_selected_row >= len => app.runs_selected_row = len - 1,
        _ => {}
    }
}

/// Handle [`Msg::OpenRank`]: switch to [`Screen::Rank`], reset any stale
/// selection/grab state, and fetch the project's full ranked ticket list.
fn open_rank(mut app: App) -> (App, Vec<Cmd>) {
    app.screen = Screen::Rank;
    app.rank_selected = 0;
    app.rank_grab_origin = None;
    app.rank_snapshot = None;
    app.status_line = "Loading priority list...".to_string();
    let query = TicketQuery::Ranked {
        project_key: app.project_key.clone(),
    };
    (app, vec![Cmd::FetchRankTickets { query }])
}

/// Handle [`Msg::RankTicketsLoaded`]: replace `rank_tickets` with server
/// truth, clearing any in-progress grab (a fresh load always reflects the
/// current server state) and preferring to keep the previously highlighted
/// ticket selected if it still exists.
fn rank_tickets_loaded(app: &mut App, tickets: Vec<TicketSummary>) {
    let preferred_key = app.rank_selected_ticket().map(|t| t.key.clone());
    app.rank_tickets = tickets;
    app.rank_grab_origin = None;
    app.rank_snapshot = None;
    let found =
        preferred_key.is_some_and(
            |key| match app.rank_tickets.iter().position(|t| t.key == key) {
                Some(pos) => {
                    app.rank_selected = pos;
                    true
                }
                None => false,
            },
        );
    if !found {
        clamp_rank_selected(app);
    }
}

/// Clamp `rank_selected` into the bounds of `rank_tickets`, resetting to `0`
/// when the list is empty.
fn clamp_rank_selected(app: &mut App) {
    match app.rank_tickets.len() {
        0 => app.rank_selected = 0,
        len if app.rank_selected >= len => app.rank_selected = len - 1,
        _ => {}
    }
}

/// Handle [`Msg::RankGrabToggle`]: grab the highlighted ticket if nothing is
/// grabbed, or drop it (emitting [`Cmd::RankTicket`] if its position
/// changed) if it is. A no-op on an empty list.
fn rank_grab_toggle(mut app: App) -> (App, Vec<Cmd>) {
    if app.rank_tickets.is_empty() {
        return (app, Vec::new());
    }

    match app.rank_grab_origin {
        None => {
            app.rank_grab_origin = Some(app.rank_selected);
            app.rank_snapshot = Some(app.rank_tickets.clone());
            (app, Vec::new())
        }
        Some(origin) => {
            app.rank_grab_origin = None;
            app.rank_snapshot = None;
            if app.rank_selected == origin {
                return (app, Vec::new());
            }
            let key = app.rank_tickets[app.rank_selected].key.clone();
            // Invariant: reaching the `None` arm below means `rank_selected`
            // is the last index (no ticket below), so `Before` isn't taken;
            // the `rank_selected == origin` check above guarantees the
            // ticket actually moved, so there must be a ticket above it too
            // (a single-item list can't move away from its own origin). Use
            // `.get` with a wrapping subtraction anyway rather than direct
            // indexing, so a future change to that invariant can't turn this
            // into a subtract-with-overflow panic.
            let anchor = match app.rank_tickets.get(app.rank_selected + 1) {
                Some(next) => Some(RankAnchor::Before(next.key.clone())),
                None => app
                    .rank_tickets
                    .get(app.rank_selected.wrapping_sub(1))
                    .map(|prev| RankAnchor::After(prev.key.clone())),
            };
            let Some(anchor) = anchor else {
                return (app, Vec::new());
            };
            (app, vec![Cmd::RankTicket { key, anchor }])
        }
    }
}

/// Handle [`Msg::Back`] while a ticket is grabbed on the rank screen: restore
/// the pre-grab order and selection, cancelling the in-progress move.
fn rank_cancel_grab(app: &mut App) {
    if let Some(origin) = app.rank_grab_origin.take()
        && let Some(snapshot) = app.rank_snapshot.take()
    {
        app.rank_tickets = snapshot;
        app.rank_selected = origin;
    }
}

/// Handle [`Msg::OpenRetro`]: switch to [`Screen::Retro`] and fetch its
/// ticket list. Resets every piece of in-progress defect-flow state, mostly
/// as defense in depth -- normal navigation can't reach `Board` with any of
/// it still set, since [`retro_recorded`]/[`retro_cancel_defect_flow`] always
/// clear it first.
fn open_retro(mut app: App) -> (App, Vec<Cmd>) {
    app.screen = Screen::Retro;
    app.retro_selected = 0;
    app.show_retro_severity_picker = false;
    app.show_retro_note_entry = false;
    app.retro_note_draft.clear();
    app.retro_action_key = None;
    app.retro_pending_severity = None;
    app.status_line = "Loading retro board...".to_string();
    let query = TicketQuery::ShippedAwaitingRetro {
        project_key: app.project_key.clone(),
    };
    (app, vec![Cmd::FetchRetroTickets { query }])
}

/// Handle [`Msg::RetroTicketsLoaded`]: replace `retro_tickets` with server
/// truth, preferring to keep the previously highlighted ticket selected if
/// it still exists, otherwise clamping into the new bounds.
fn retro_tickets_loaded(app: &mut App, tickets: Vec<RetroRow>) {
    let preferred_key = app.retro_selected_ticket().map(|t| t.key.clone());
    app.retro_tickets = tickets;
    let found = preferred_key.is_some_and(|key| {
        match app.retro_tickets.iter().position(|t| t.key == key) {
            Some(pos) => {
                app.retro_selected = pos;
                true
            }
            None => false,
        }
    });
    if !found {
        clamp_retro_selected(app);
    }
}

/// Clamp `retro_selected` into the bounds of `retro_tickets`, resetting to
/// `0` when the list is empty.
fn clamp_retro_selected(app: &mut App) {
    match app.retro_tickets.len() {
        0 => app.retro_selected = 0,
        len if app.retro_selected >= len => app.retro_selected = len - 1,
        _ => {}
    }
}

/// Handle [`Msg::RetroDefectStart`]: capture the highlighted ticket's key
/// and open the severity picker, highlighting [`RetroSeverity::Minor`]
/// first. A no-op when nothing is highlighted.
fn retro_defect_start(mut app: App) -> (App, Vec<Cmd>) {
    let Some(ticket) = app.retro_selected_ticket() else {
        return (app, Vec::new());
    };
    app.retro_action_key = Some(ticket.key.clone());
    app.show_retro_severity_picker = true;
    app.retro_severity_selected = 0;
    (app, Vec::new())
}

/// Handle [`Msg::RetroSeverityPickerSelect`]: record the highlighted
/// severity and move on to the note-entry step. A no-op (picker stays open)
/// if `retro_action_key` is somehow unset -- defense in depth, since
/// [`retro_defect_start`] is the only way to set `show_retro_severity_picker`
/// and always sets it alongside `retro_action_key`.
fn retro_severity_picker_select(mut app: App) -> (App, Vec<Cmd>) {
    if app.retro_action_key.is_none() {
        return (app, Vec::new());
    }
    let severity = RETRO_SEVERITIES[app.retro_severity_selected.min(RETRO_SEVERITIES.len() - 1)];
    app.retro_pending_severity = Some(severity);
    app.show_retro_severity_picker = false;
    app.show_retro_note_entry = true;
    app.retro_note_draft.clear();
    (app, Vec::new())
}

/// Discard an in-progress defect flow (from either the severity picker or
/// the note-entry step): the ticket stays on the board, nothing is recorded.
fn retro_cancel_defect_flow(app: &mut App) {
    app.show_retro_severity_picker = false;
    app.show_retro_note_entry = false;
    app.retro_note_draft.clear();
    app.retro_action_key = None;
    app.retro_pending_severity = None;
}

/// Handle [`Msg::RetroNoteSubmit`]: record [`RetroVerdict::Defect`] with the
/// severity chosen earlier in the flow and whatever note text has been
/// typed (trimmed; empty becomes `None` rather than recording a blank
/// note). A no-op (clears the flow with nothing recorded) if
/// `retro_action_key`/`retro_pending_severity` are somehow unset --
/// [`retro_severity_picker_select`] always sets both before
/// `show_retro_note_entry` goes up.
fn retro_note_submit(mut app: App) -> (App, Vec<Cmd>) {
    let key = app.retro_action_key.take();
    let severity = app.retro_pending_severity.take();
    let notes = std::mem::take(&mut app.retro_note_draft);
    app.show_retro_note_entry = false;

    let (Some(key), Some(severity)) = (key, severity) else {
        return (app, Vec::new());
    };
    let notes = {
        let trimmed = notes.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    };
    (
        app,
        vec![Cmd::RecordRetro {
            key,
            verdict: RetroVerdict::Defect,
            severity: Some(severity),
            notes,
        }],
    )
}

/// Handle [`Msg::RetroMarkClean`]: record [`RetroVerdict::Clean`] for the
/// highlighted ticket directly, no picker. A no-op when nothing is
/// highlighted.
fn retro_mark_clean(app: App) -> (App, Vec<Cmd>) {
    let Some(ticket) = app.retro_selected_ticket() else {
        return (app, Vec::new());
    };
    let key = ticket.key.clone();
    (
        app,
        vec![Cmd::RecordRetro {
            key,
            verdict: RetroVerdict::Clean,
            severity: None,
            notes: None,
        }],
    )
}

/// Handle [`Msg::RetroRecorded`]: drop `key` from `retro_tickets` (it now
/// has a verdict, so it's no longer awaiting one) and report it in the
/// status line. Clamps `retro_selected` into the shrunk list's bounds.
fn retro_recorded(app: &mut App, key: &str, verdict: RetroVerdict) {
    app.retro_tickets.retain(|t| t.key != key);
    clamp_retro_selected(app);
    app.status_line = format!("Recorded {} for {key}", verdict.as_str());
}

/// Handle [`Msg::OpenFilterPicker`]: show the picker, highlight the currently
/// active filter, and fetch assignable users if they haven't been cached yet.
fn open_filter_picker(mut app: App) -> (App, Vec<Cmd>) {
    app.show_filter_picker = true;
    app.filter_picker_error = None;
    app.filter_picker_selected = app
        .filter_options()
        .iter()
        .position(|option| option == &app.filter)
        .unwrap_or(0);

    let cmds = if app.assignable_users.is_none() {
        vec![Cmd::FetchAssignableUsers {
            project: app.project_key.clone(),
        }]
    } else {
        Vec::new()
    };
    (app, cmds)
}

/// Handle [`Msg::FilterPickerSelect`]: apply the highlighted option as the
/// active filter, close the picker, and refetch tickets under the new
/// filter.
fn filter_picker_select(mut app: App) -> (App, Vec<Cmd>) {
    let Some(filter) = app
        .filter_options()
        .get(app.filter_picker_selected)
        .cloned()
    else {
        return (app, Vec::new());
    };
    app.filter = filter;
    app.show_filter_picker = false;
    app.status_line = "Refreshing...".to_string();
    let query = query_for_filter(&app.filter, &app.project_key);
    (app, vec![Cmd::FetchTickets { query }])
}

/// Handle [`Msg::OpenAssignPicker`]: show the picker for the selected card,
/// and fetch assignable users if they haven't been cached yet. A no-op if no
/// ticket is selected (e.g. an empty board).
fn open_assign_picker(mut app: App) -> (App, Vec<Cmd>) {
    let Some(ticket) = app.selected_ticket() else {
        return (app, Vec::new());
    };
    app.assign_picker_key = Some(ticket.key.clone());
    app.show_assign_picker = true;
    app.assign_picker_error = None;
    app.assign_picker_selected = 0;

    let cmds = if app.assignable_users.is_none() {
        vec![Cmd::FetchAssignableUsers {
            project: app.project_key.clone(),
        }]
    } else {
        Vec::new()
    };
    (app, cmds)
}

/// Handle [`Msg::AssignPickerSelect`]: apply the highlighted choice to the
/// ticket the picker was opened on, and close the picker. A no-op if the
/// selection is out of range or the picker's target ticket is somehow unset.
fn assign_picker_select(mut app: App) -> (App, Vec<Cmd>) {
    let Some(choice) = app
        .assign_options()
        .get(app.assign_picker_selected)
        .cloned()
    else {
        return (app, Vec::new());
    };
    let Some(key) = app.assign_picker_key.take() else {
        return (app, Vec::new());
    };
    app.show_assign_picker = false;
    app.status_line = format!("Assigning {key}...");
    (app, vec![Cmd::AssignTicket { key, choice }])
}

/// Handle [`Msg::Enter`]: activate whatever is selected on the current
/// screen.
fn enter(mut app: App) -> (App, Vec<Cmd>) {
    match app.screen {
        Screen::Board => {
            if app.selected_ticket().is_some() {
                app.screen = Screen::Detail;
            }
            (app, Vec::new())
        }
        Screen::Detail => match app.selected_ticket() {
            Some(ticket) => {
                let key = ticket.key.clone();
                (app, vec![Cmd::FetchTransitions { key }])
            }
            None => (app, Vec::new()),
        },
        Screen::TransitionMenu => {
            let cmd = match (
                app.selected_ticket(),
                app.transitions.get(app.transition_selected),
            ) {
                (Some(ticket), Some(transition)) => Some(Cmd::ApplyTransition {
                    key: ticket.key.clone(),
                    transition_id: transition.id.clone(),
                }),
                _ => None,
            };
            (app, cmd.into_iter().collect())
        }
        // `map_key` routes Enter/Space on the rank screen to
        // `Msg::RankGrabToggle`, never `Msg::Enter`; kept as a no-op so
        // `Screen` stays exhaustively matched here.
        Screen::Rank => (app, Vec::new()),
        // `map_key` never emits `Msg::Enter` while `show_run_detail` is set,
        // so this only fires with the detail window closed.
        Screen::Runs => match app.selected_run_card() {
            Some(card) => {
                let run_id = card.id;
                app.show_run_detail = true;
                app.run_detail = None;
                app.run_detail_scroll = 0;
                (app, vec![Cmd::LoadRunDetail { run_id }])
            }
            None => (app, Vec::new()),
        },
        // `d`/`c` are the retro board's actions; `Enter` has nothing to
        // drill into here, kept as a no-op so `Screen` stays exhaustively
        // matched.
        Screen::Retro => (app, Vec::new()),
    }
}

/// Handle [`Msg::Back`]: step back a screen, or quit from the board. On the
/// rank screen, cancels an in-progress grab instead of leaving the screen if
/// one is active (so `Esc`/`q` can never quit or navigate away mid-grab). On
/// the runs screen, closes the detail window if one is open instead of
/// quitting (`tm runs watch` has no screen to fall back to, so `Back` quits
/// only once the window is already closed). The board mirrors that: if the
/// run detail overlay ([`Msg::ViewRunAction`]) is open, `Back` closes it
/// instead of quitting.
fn back(app: &mut App) {
    match app.screen {
        Screen::Board => {
            if app.show_run_detail {
                app.show_run_detail = false;
                app.run_detail = None;
            } else {
                app.quit = true;
            }
        }
        Screen::Detail => app.screen = Screen::Board,
        Screen::TransitionMenu => app.screen = Screen::Detail,
        Screen::Rank => {
            if app.is_rank_grabbed() {
                rank_cancel_grab(app);
            } else {
                app.screen = Screen::Board;
            }
        }
        Screen::Runs => {
            if app.show_run_detail {
                app.show_run_detail = false;
                app.run_detail = None;
            } else {
                app.quit = true;
            }
        }
        // `map_key` routes Esc/`q` to `Msg::RetroSeverityPickerClose`/
        // `Msg::RetroNoteCancel` while either overlay is open, so `Back`
        // only ever fires here with both closed.
        Screen::Retro => app.screen = Screen::Board,
    }
}

/// Move the current selection/scroll up by one, saturating at the top. On
/// the rank screen, moves the grabbed ticket itself (cursor follows it)
/// instead of just the cursor while a grab is active.
fn move_up(app: &mut App) {
    match app.screen {
        Screen::Board => {
            if app.show_run_detail {
                app.run_detail_scroll = app.run_detail_scroll.saturating_sub(1);
            } else {
                app.selected_row = app.selected_row.saturating_sub(1);
            }
        }
        Screen::Detail => app.detail_scroll = app.detail_scroll.saturating_sub(1),
        Screen::TransitionMenu => {
            app.transition_selected = app.transition_selected.saturating_sub(1);
        }
        Screen::Rank => {
            if app.is_rank_grabbed() {
                rank_swap_up(app);
            } else {
                app.rank_selected = app.rank_selected.saturating_sub(1);
            }
        }
        Screen::Runs => {
            if app.show_run_detail {
                app.run_detail_scroll = app.run_detail_scroll.saturating_sub(1);
            } else {
                app.runs_selected_row = app.runs_selected_row.saturating_sub(1);
            }
        }
        Screen::Retro => {
            app.retro_selected = app.retro_selected.saturating_sub(1);
        }
    }
}

/// Move the current selection/scroll down by one, clamping at the bottom of
/// the relevant list. Detail scroll has no known upper bound at this layer,
/// so it is left to increase; the detail view clamps what it displays.
fn move_down(app: &mut App) {
    match app.screen {
        Screen::Board => {
            if app.show_run_detail {
                app.run_detail_scroll = app.run_detail_scroll.saturating_add(1);
            } else if let Some(len) = current_column_len(app) {
                app.selected_row = (app.selected_row + 1).min(len - 1);
            }
        }
        Screen::Detail => app.detail_scroll = app.detail_scroll.saturating_add(1),
        Screen::TransitionMenu => {
            if !app.transitions.is_empty() {
                app.transition_selected =
                    (app.transition_selected + 1).min(app.transitions.len() - 1);
            }
        }
        Screen::Rank => {
            if app.is_rank_grabbed() {
                rank_swap_down(app);
            } else if !app.rank_tickets.is_empty() {
                app.rank_selected = (app.rank_selected + 1).min(app.rank_tickets.len() - 1);
            }
        }
        Screen::Runs => {
            if app.show_run_detail {
                app.run_detail_scroll = app.run_detail_scroll.saturating_add(1);
            } else {
                let len = app.runs_in_col(app.runs_selected_col).len();
                if len > 0 {
                    app.runs_selected_row = (app.runs_selected_row + 1).min(len - 1);
                }
            }
        }
        Screen::Retro => {
            if !app.retro_tickets.is_empty() {
                app.retro_selected = (app.retro_selected + 1).min(app.retro_tickets.len() - 1);
            }
        }
    }
}

/// Swap the grabbed ticket with its upstairs neighbor and follow it with the
/// cursor, clamping (no-op) at the top of the list.
fn rank_swap_up(app: &mut App) {
    if app.rank_selected == 0 {
        return;
    }
    app.rank_tickets
        .swap(app.rank_selected, app.rank_selected - 1);
    app.rank_selected -= 1;
}

/// Swap the grabbed ticket with its downstairs neighbor and follow it with
/// the cursor, clamping (no-op) at the bottom of the list.
fn rank_swap_down(app: &mut App) {
    if app.rank_tickets.is_empty() || app.rank_selected >= app.rank_tickets.len() - 1 {
        return;
    }
    app.rank_tickets
        .swap(app.rank_selected, app.rank_selected + 1);
    app.rank_selected += 1;
}

/// Move the selected column left by one, saturating at the first column. A
/// no-op outside [`Screen::Board`]/[`Screen::Runs`], on an empty board, or
/// while the run detail window is open.
fn move_left(app: &mut App) {
    match app.screen {
        Screen::Board if !app.columns.is_empty() => {
            app.selected_col = app.selected_col.saturating_sub(1);
            clamp_row(app);
        }
        Screen::Runs if !app.show_run_detail => {
            app.runs_selected_col = app.runs_selected_col.saturating_sub(1);
            clamp_runs_row(app);
        }
        _ => {}
    }
}

/// Move the selected column right by one, clamping at the last column. A
/// no-op outside [`Screen::Board`]/[`Screen::Runs`], on an empty board, or
/// while the run detail window is open.
fn move_right(app: &mut App) {
    match app.screen {
        Screen::Board if !app.columns.is_empty() => {
            app.selected_col = (app.selected_col + 1).min(app.columns.len() - 1);
            clamp_row(app);
        }
        Screen::Runs if !app.show_run_detail => {
            app.runs_selected_col = (app.runs_selected_col + 1).min(RUN_COLUMNS.len() - 1);
            clamp_runs_row(app);
        }
        _ => {}
    }
}

/// The number of tickets in the currently selected column, or `None` if
/// there are no columns.
fn current_column_len(app: &App) -> Option<usize> {
    app.columns.get(app.selected_col).map(|c| c.tickets.len())
}

/// Clamp `selected_row` into the bounds of the currently selected column,
/// resetting to `0` when that column is empty.
fn clamp_row(app: &mut App) {
    match current_column_len(app) {
        Some(0) | None => app.selected_row = 0,
        Some(len) if app.selected_row >= len => app.selected_row = len - 1,
        Some(_) => {}
    }
}

/// Flatten every column's tickets back into a single list, in column then
/// fetch order.
fn flatten(columns: &[Column]) -> Vec<TicketSummary> {
    columns.iter().flat_map(|c| c.tickets.clone()).collect()
}

/// Select the ticket with key `key`, if it exists in `app.columns`. Returns
/// whether it was found.
fn select_by_key(app: &mut App, key: &str) -> bool {
    for (col_index, column) in app.columns.iter().enumerate() {
        if let Some(row_index) = column.tickets.iter().position(|t| t.key == key) {
            app.selected_col = col_index;
            app.selected_row = row_index;
            return true;
        }
    }
    false
}

/// Re-establish selection after `app.columns` has been rebuilt: prefer
/// keeping `preferred_key` selected if it still exists, otherwise clamp the
/// existing indices into the new bounds.
fn reselect(app: &mut App, preferred_key: Option<String>) {
    let found = preferred_key.is_some_and(|key| select_by_key(app, &key));
    if !found {
        clamp_selection(app);
    }
}

/// Clamp `selected_col`/`selected_row` into the bounds of `columns`,
/// resetting both to `0` when `columns` is empty.
fn clamp_selection(app: &mut App) {
    if app.columns.is_empty() {
        app.selected_col = 0;
        app.selected_row = 0;
        return;
    }
    if app.selected_col >= app.columns.len() {
        app.selected_col = app.columns.len() - 1;
    }
    clamp_row(app);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket(key: &str) -> TicketSummary {
        TicketSummary {
            key: key.to_string(),
            summary: format!("Summary for {key}"),
            status: "To Do".to_string(),
            url: format!("https://example.atlassian.net/browse/{key}"),
            description: format!("Description for {key}"),
            status_category: "new".to_string(),
            assignee: None,
        }
    }

    fn ticket_with(key: &str, status: &str, status_category: &str) -> TicketSummary {
        TicketSummary {
            status: status.to_string(),
            status_category: status_category.to_string(),
            ..ticket(key)
        }
    }

    fn board_with(tickets: Vec<TicketSummary>, selected_row: usize) -> App {
        App {
            columns: group_into_columns(tickets, &[]),
            selected_col: 0,
            selected_row,
            // The canonical test scope slug: attach-path reducers build
            // session names as `tm-proj-<lowercased key>`.
            session_slug: "proj".to_string(),
            ..App::new()
        }
    }

    #[test]
    fn up_on_board_is_a_noop_when_empty() {
        let app = board_with(vec![], 0);
        let (app, cmds) = update(app, Msg::Up);
        assert_eq!(app.selected_row, 0);
        assert!(cmds.is_empty());
    }

    #[test]
    fn down_on_board_is_a_noop_when_empty() {
        let app = board_with(vec![], 0);
        let (app, cmds) = update(app, Msg::Down);
        assert_eq!(app.selected_row, 0);
        assert!(cmds.is_empty());
    }

    #[test]
    fn up_clamps_at_zero() {
        let app = board_with(vec![ticket("PROJ-1"), ticket("PROJ-2")], 0);
        let (app, _) = update(app, Msg::Up);
        assert_eq!(app.selected_row, 0);
    }

    #[test]
    fn down_clamps_at_last_index() {
        let app = board_with(vec![ticket("PROJ-1"), ticket("PROJ-2")], 1);
        let (app, _) = update(app, Msg::Down);
        assert_eq!(app.selected_row, 1);
    }

    #[test]
    fn up_and_down_move_selection_within_bounds() {
        let app = board_with(
            vec![ticket("PROJ-1"), ticket("PROJ-2"), ticket("PROJ-3")],
            1,
        );
        let (app, _) = update(app, Msg::Down);
        assert_eq!(app.selected_row, 2);
        let (app, _) = update(app, Msg::Up);
        assert_eq!(app.selected_row, 1);
    }

    #[test]
    fn left_and_right_are_noops_when_board_has_one_column() {
        let app = board_with(vec![ticket("PROJ-1"), ticket("PROJ-2")], 1);
        let (app, _) = update(app, Msg::Right);
        assert_eq!(app.selected_col, 0);
        assert_eq!(app.selected_row, 1);
        let (app, _) = update(app, Msg::Left);
        assert_eq!(app.selected_col, 0);
        assert_eq!(app.selected_row, 1);
    }

    #[test]
    fn left_and_right_move_between_columns_and_clamp() {
        let tickets = vec![
            ticket_with("PROJ-1", "To Do", "new"),
            ticket_with("PROJ-2", "In Progress", "indeterminate"),
            ticket_with("PROJ-3", "In Progress", "indeterminate"),
            ticket_with("PROJ-4", "Done", "done"),
        ];
        let app = App {
            columns: group_into_columns(tickets, &[]),
            selected_col: 0,
            selected_row: 0,
            ..App::new()
        };

        // To Do (1 ticket) -> In Progress (2 tickets): row stays 0.
        let (app, _) = update(app, Msg::Right);
        assert_eq!(app.selected_col, 1);
        assert_eq!(app.selected_row, 0);

        // Move to the second ticket in In Progress, then right into Done
        // (1 ticket): row must clamp down to 0.
        let (app, _) = update(app, Msg::Down);
        assert_eq!(app.selected_row, 1);
        let (app, _) = update(app, Msg::Right);
        assert_eq!(app.selected_col, 2);
        assert_eq!(app.selected_row, 0);

        // Right again clamps at the last column.
        let (app, _) = update(app, Msg::Right);
        assert_eq!(app.selected_col, 2);

        // Left steps back through columns.
        let (app, _) = update(app, Msg::Left);
        assert_eq!(app.selected_col, 1);
        let (app, _) = update(app, Msg::Left);
        assert_eq!(app.selected_col, 0);
        let (app, _) = update(app, Msg::Left);
        assert_eq!(app.selected_col, 0);
    }

    #[test]
    fn left_and_right_are_noops_when_board_is_empty() {
        let app = board_with(vec![], 0);
        let (app, cmds) = update(app, Msg::Right);
        assert_eq!(app.selected_col, 0);
        assert!(cmds.is_empty());
        let (app, cmds) = update(app, Msg::Left);
        assert_eq!(app.selected_col, 0);
        assert!(cmds.is_empty());
    }

    #[test]
    fn left_and_right_are_ignored_off_the_board_screen() {
        let app = App {
            screen: Screen::Detail,
            detail_scroll: 3,
            ..board_with(vec![ticket("PROJ-1")], 0)
        };
        let (app, _) = update(app, Msg::Right);
        assert_eq!(app.selected_col, 0);
        assert_eq!(app.detail_scroll, 3);
    }

    #[test]
    fn refresh_sets_status_line_and_emits_fetch_tickets() {
        let app = App::new();
        let (app, cmds) = update(app, Msg::Refresh);
        assert_eq!(app.status_line, "Refreshing...");
        assert_eq!(
            cmds,
            vec![Cmd::FetchTickets {
                query: TicketQuery::MyOpen
            }]
        );
    }

    #[test]
    fn tickets_loaded_replaces_list_and_clamps_selected() {
        let app = board_with(
            vec![ticket("PROJ-1"), ticket("PROJ-2"), ticket("PROJ-3")],
            2,
        );
        let (app, cmds) = update(app, Msg::TicketsLoaded(vec![ticket("PROJ-9")]));
        assert_eq!(
            app.columns,
            vec![Column {
                title: "To Do".to_string(),
                tickets: vec![ticket("PROJ-9")],
            }]
        );
        assert_eq!(app.selected_col, 0);
        assert_eq!(app.selected_row, 0);
        assert!(cmds.is_empty());
    }

    #[test]
    fn tickets_loaded_with_empty_list_resets_selected_to_zero() {
        let app = board_with(vec![ticket("PROJ-1")], 0);
        let (app, _) = update(app, Msg::TicketsLoaded(vec![]));
        assert_eq!(app.selected_col, 0);
        assert_eq!(app.selected_row, 0);
        assert!(app.columns.is_empty());
    }

    #[test]
    fn tickets_loaded_preserves_selection_by_key_when_still_present() {
        let app = board_with(vec![ticket("PROJ-1"), ticket("PROJ-2")], 1);
        let (app, _) = update(
            app,
            Msg::TicketsLoaded(vec![ticket("PROJ-0"), ticket("PROJ-2"), ticket("PROJ-3")]),
        );
        assert_eq!(app.selected_ticket().unwrap().key, "PROJ-2");
    }

    #[test]
    fn tickets_failed_sets_status_line() {
        let app = App::new();
        let (app, cmds) = update(app, Msg::TicketsFailed("boom".to_string()));
        assert_eq!(app.status_line, "boom");
        assert!(cmds.is_empty());
    }

    #[test]
    fn search_truncated_warns_in_the_status_line_with_the_count_shown() {
        let app = App::new();
        let (app, cmds) = update(app, Msg::SearchTruncated { shown: 500 });
        assert!(
            app.status_line.contains("500"),
            "status line should name how many tickets are shown, got {:?}",
            app.status_line
        );
        assert!(
            app.status_line.contains("narrow"),
            "status line should tell the user what to do about it, got {:?}",
            app.status_line
        );
        assert!(cmds.is_empty());
    }

    #[test]
    fn quit_sets_quit_flag() {
        let app = App::new();
        let (app, cmds) = update(app, Msg::Quit);
        assert!(app.quit);
        assert!(cmds.is_empty());
    }

    #[test]
    fn open_in_browser_emits_open_url_for_selected_ticket() {
        let app = board_with(vec![ticket("PROJ-1"), ticket("PROJ-2")], 1);
        let (_, cmds) = update(app, Msg::OpenInBrowser);
        assert_eq!(
            cmds,
            vec![Cmd::OpenUrl(
                "https://example.atlassian.net/browse/PROJ-2".to_string()
            )]
        );
    }

    #[test]
    fn open_in_browser_with_no_tickets_emits_nothing() {
        let app = board_with(vec![], 0);
        let (_, cmds) = update(app, Msg::OpenInBrowser);
        assert!(cmds.is_empty());
    }

    fn pr_info(number: u64, url: &str) -> crate::github::pr::PrInfo {
        crate::github::pr::PrInfo {
            number,
            url: url.to_string(),
            title: format!("[PROJ-1] PR {number}"),
            body: String::new(),
            head_ref_name: "proj-1-fix".to_string(),
        }
    }

    #[test]
    fn open_browser_action_emits_resolve_pr_for_selected_ticket() {
        let app = board_with(vec![ticket("PROJ-1"), ticket("PROJ-2")], 1);
        let (_, cmds) = update(app, Msg::OpenBrowserAction);
        assert_eq!(
            cmds,
            vec![Cmd::ResolvePrForTicket {
                key: "PROJ-2".to_string(),
                jira_url: "https://example.atlassian.net/browse/PROJ-2".to_string(),
            }]
        );
    }

    #[test]
    fn open_browser_action_sets_resolving_status_line() {
        let app = board_with(vec![ticket("PROJ-1"), ticket("PROJ-2")], 1);
        let (app, _) = update(app, Msg::OpenBrowserAction);
        assert_eq!(app.status_line, "resolving PR for PROJ-2...");
    }

    #[test]
    fn open_browser_action_with_no_tickets_emits_nothing() {
        let app = board_with(vec![], 0);
        let (_, cmds) = update(app, Msg::OpenBrowserAction);
        assert!(cmds.is_empty());
    }

    #[test]
    fn open_browser_action_off_board_is_a_noop() {
        let app = App {
            screen: Screen::Detail,
            ..board_with(vec![ticket("PROJ-1")], 0)
        };
        let (_, cmds) = update(app, Msg::OpenBrowserAction);
        assert!(cmds.is_empty());
    }

    #[test]
    fn browser_options_resolved_with_no_pr_opens_jira_directly() {
        let app = App::new();
        let (app, cmds) = update(
            app,
            Msg::BrowserOptionsResolved {
                key: "PROJ-1".to_string(),
                jira_url: "https://example.atlassian.net/browse/PROJ-1".to_string(),
                pr: None,
                note: None,
            },
        );
        assert!(!app.show_browser_picker);
        assert_eq!(
            cmds,
            vec![Cmd::OpenUrl(
                "https://example.atlassian.net/browse/PROJ-1".to_string()
            )]
        );
    }

    #[test]
    fn browser_options_resolved_with_timeout_note_opens_jira_and_sets_status_line() {
        let app = App::new();
        let (app, cmds) = update(
            app,
            Msg::BrowserOptionsResolved {
                key: "PROJ-1".to_string(),
                jira_url: "https://example.atlassian.net/browse/PROJ-1".to_string(),
                pr: None,
                note: Some("PR lookup for PROJ-1 timed out; opening Jira".to_string()),
            },
        );
        assert!(!app.show_browser_picker);
        assert_eq!(
            app.status_line,
            "PR lookup for PROJ-1 timed out; opening Jira"
        );
        assert_eq!(
            cmds,
            vec![Cmd::OpenUrl(
                "https://example.atlassian.net/browse/PROJ-1".to_string()
            )]
        );
    }

    #[test]
    fn browser_options_resolved_with_pr_shows_picker_with_both_options() {
        let app = App::new();
        let (app, cmds) = update(
            app,
            Msg::BrowserOptionsResolved {
                key: "PROJ-1".to_string(),
                jira_url: "https://example.atlassian.net/browse/PROJ-1".to_string(),
                pr: Some(pr_info(42, "https://github.com/example/repo/pull/42")),
                note: None,
            },
        );
        assert!(app.show_browser_picker);
        assert_eq!(app.browser_picker_selected, 0);
        assert_eq!(
            app.browser_picker_options,
            vec![
                BrowserPickerOption::Jira {
                    key: "PROJ-1".to_string(),
                    url: "https://example.atlassian.net/browse/PROJ-1".to_string(),
                },
                BrowserPickerOption::GitHub {
                    number: 42,
                    url: "https://github.com/example/repo/pull/42".to_string(),
                },
            ]
        );
        assert!(cmds.is_empty());
    }

    #[test]
    fn browser_picker_option_labels() {
        let jira = BrowserPickerOption::Jira {
            key: "PROJ-1".to_string(),
            url: "https://example.atlassian.net/browse/PROJ-1".to_string(),
        };
        assert_eq!(jira.label(), "Jira (PROJ-1)");
        let github = BrowserPickerOption::GitHub {
            number: 42,
            url: "https://github.com/example/repo/pull/42".to_string(),
        };
        assert_eq!(github.label(), "GitHub (#42)");
    }

    fn app_with_browser_picker(options: Vec<BrowserPickerOption>, selected: usize) -> App {
        App {
            show_browser_picker: true,
            browser_picker_options: options,
            browser_picker_selected: selected,
            ..App::new()
        }
    }

    #[test]
    fn browser_picker_up_and_down_move_within_bounds() {
        let app = app_with_browser_picker(
            vec![
                BrowserPickerOption::Jira {
                    key: "PROJ-1".to_string(),
                    url: "https://example.atlassian.net/browse/PROJ-1".to_string(),
                },
                BrowserPickerOption::GitHub {
                    number: 42,
                    url: "https://github.com/example/repo/pull/42".to_string(),
                },
            ],
            0,
        );
        let (app, _) = update(app, Msg::BrowserPickerDown);
        assert_eq!(app.browser_picker_selected, 1);
        let (app, _) = update(app, Msg::BrowserPickerDown);
        assert_eq!(app.browser_picker_selected, 1);
        let (app, _) = update(app, Msg::BrowserPickerUp);
        assert_eq!(app.browser_picker_selected, 0);
        let (app, _) = update(app, Msg::BrowserPickerUp);
        assert_eq!(app.browser_picker_selected, 0);
    }

    #[test]
    fn browser_picker_select_opens_highlighted_option_and_closes_picker() {
        let app = app_with_browser_picker(
            vec![
                BrowserPickerOption::Jira {
                    key: "PROJ-1".to_string(),
                    url: "https://example.atlassian.net/browse/PROJ-1".to_string(),
                },
                BrowserPickerOption::GitHub {
                    number: 42,
                    url: "https://github.com/example/repo/pull/42".to_string(),
                },
            ],
            1,
        );
        let (app, cmds) = update(app, Msg::BrowserPickerSelect);
        assert!(!app.show_browser_picker);
        assert_eq!(
            cmds,
            vec![Cmd::OpenUrl(
                "https://github.com/example/repo/pull/42".to_string()
            )]
        );
    }

    #[test]
    fn browser_picker_select_out_of_range_is_a_noop() {
        let app = app_with_browser_picker(vec![], 0);
        let (app, cmds) = update(app, Msg::BrowserPickerSelect);
        assert!(app.show_browser_picker);
        assert!(cmds.is_empty());
    }

    #[test]
    fn browser_picker_close_hides_picker_without_a_cmd() {
        let app = app_with_browser_picker(
            vec![BrowserPickerOption::Jira {
                key: "PROJ-1".to_string(),
                url: "https://example.atlassian.net/browse/PROJ-1".to_string(),
            }],
            0,
        );
        let (app, cmds) = update(app, Msg::BrowserPickerClose);
        assert!(!app.show_browser_picker);
        assert!(cmds.is_empty());
    }

    #[test]
    fn toggle_help_flips_show_help() {
        let app = App::new();
        let (app, _) = update(app, Msg::ToggleHelp);
        assert!(app.show_help);
        let (app, _) = update(app, Msg::ToggleHelp);
        assert!(!app.show_help);
    }

    fn transition(id: &str, name: &str) -> Transition {
        use crate::ticketing::types::{Status, StatusCategory};

        Transition {
            id: id.to_string(),
            name: name.to_string(),
            to: Status {
                name: name.to_string(),
                status_category: StatusCategory {
                    key: "indeterminate".to_string(),
                },
            },
        }
    }

    #[test]
    fn enter_on_board_with_selection_opens_detail() {
        let app = board_with(vec![ticket("PROJ-1")], 0);
        let (app, cmds) = update(app, Msg::Enter);
        assert_eq!(app.screen, Screen::Detail);
        assert!(cmds.is_empty());
    }

    #[test]
    fn enter_on_board_with_no_tickets_stays_on_board() {
        let app = board_with(vec![], 0);
        let (app, cmds) = update(app, Msg::Enter);
        assert_eq!(app.screen, Screen::Board);
        assert!(cmds.is_empty());
    }

    #[test]
    fn enter_on_detail_emits_fetch_transitions_and_stays_on_detail() {
        let app = App {
            screen: Screen::Detail,
            ..board_with(vec![ticket("PROJ-1")], 0)
        };
        let (app, cmds) = update(app, Msg::Enter);
        assert_eq!(app.screen, Screen::Detail);
        assert_eq!(
            cmds,
            vec![Cmd::FetchTransitions {
                key: "PROJ-1".to_string()
            }]
        );
    }

    #[test]
    fn transitions_loaded_moves_to_transition_menu() {
        let app = App {
            screen: Screen::Detail,
            ..board_with(vec![ticket("PROJ-1")], 0)
        };
        let (app, cmds) = update(
            app,
            Msg::TransitionsLoaded(vec![transition("11", "Start Progress")]),
        );
        assert_eq!(app.screen, Screen::TransitionMenu);
        assert_eq!(app.transitions, vec![transition("11", "Start Progress")]);
        assert_eq!(app.transition_selected, 0);
        assert!(cmds.is_empty());
    }

    #[test]
    fn transitions_failed_sets_status_line_and_stays_on_detail() {
        let app = App {
            screen: Screen::Detail,
            ..board_with(vec![ticket("PROJ-1")], 0)
        };
        let (app, cmds) = update(app, Msg::TransitionsFailed("boom".to_string()));
        assert_eq!(app.status_line, "boom");
        assert_eq!(app.screen, Screen::Detail);
        assert!(cmds.is_empty());
    }

    #[test]
    fn enter_on_transition_menu_emits_apply_transition() {
        let app = App {
            screen: Screen::TransitionMenu,
            transitions: vec![transition("11", "Start Progress")],
            transition_selected: 0,
            ..board_with(vec![ticket("PROJ-1")], 0)
        };
        let (_, cmds) = update(app, Msg::Enter);
        assert_eq!(
            cmds,
            vec![Cmd::ApplyTransition {
                key: "PROJ-1".to_string(),
                transition_id: "11".to_string()
            }]
        );
    }

    #[test]
    fn transition_applied_updates_ticket_status_and_returns_to_detail() {
        let app = App {
            screen: Screen::TransitionMenu,
            transitions: vec![transition("11", "In Progress")],
            ..board_with(vec![ticket("PROJ-1"), ticket("PROJ-2")], 0)
        };
        let (app, cmds) = update(
            app,
            Msg::TransitionApplied {
                key: "PROJ-1".to_string(),
                status: "In Progress".to_string(),
                status_category: "indeterminate".to_string(),
            },
        );
        assert_eq!(app.screen, Screen::Detail);
        assert_eq!(app.status_line, "PROJ-1 -> In Progress");
        assert_eq!(
            flatten(&app.columns)
                .iter()
                .find(|t| t.key == "PROJ-1")
                .unwrap()
                .status,
            "In Progress"
        );
        assert!(cmds.is_empty());
    }

    #[test]
    fn transition_applied_moves_ticket_across_columns_and_selection_follows_it() {
        let tickets = vec![
            ticket_with("PROJ-1", "To Do", "new"),
            ticket_with("PROJ-2", "To Do", "new"),
        ];
        let app = App {
            screen: Screen::TransitionMenu,
            transitions: vec![transition("11", "Done")],
            columns: group_into_columns(tickets, &[]),
            selected_col: 0,
            selected_row: 0,
            ..App::new()
        };

        let (app, _) = update(
            app,
            Msg::TransitionApplied {
                key: "PROJ-1".to_string(),
                status: "Done".to_string(),
                status_category: "done".to_string(),
            },
        );

        // PROJ-1 leaves the "To Do" column (now down to just PROJ-2) and lands
        // in a new "Done" column, ordered after "To Do" (new < done).
        assert_eq!(
            app.columns,
            vec![
                Column {
                    title: "To Do".to_string(),
                    tickets: vec![ticket_with("PROJ-2", "To Do", "new")],
                },
                Column {
                    title: "Done".to_string(),
                    tickets: vec![ticket_with("PROJ-1", "Done", "done")],
                },
            ]
        );
        // Selection follows PROJ-1 into its new column.
        assert_eq!(app.selected_col, 1);
        assert_eq!(app.selected_row, 0);
        assert_eq!(app.selected_ticket().unwrap().key, "PROJ-1");
    }

    #[test]
    fn transition_failed_sets_status_line() {
        let app = App {
            screen: Screen::TransitionMenu,
            ..board_with(vec![ticket("PROJ-1")], 0)
        };
        let (app, cmds) = update(app, Msg::TransitionFailed("nope".to_string()));
        assert_eq!(app.status_line, "nope");
        assert!(cmds.is_empty());
    }

    #[test]
    fn back_on_transition_menu_returns_to_detail() {
        let app = App {
            screen: Screen::TransitionMenu,
            ..board_with(vec![ticket("PROJ-1")], 0)
        };
        let (app, _) = update(app, Msg::Back);
        assert_eq!(app.screen, Screen::Detail);
    }

    #[test]
    fn back_on_detail_returns_to_board() {
        let app = App {
            screen: Screen::Detail,
            ..board_with(vec![ticket("PROJ-1")], 0)
        };
        let (app, _) = update(app, Msg::Back);
        assert_eq!(app.screen, Screen::Board);
    }

    #[test]
    fn back_on_board_quits() {
        let app = board_with(vec![ticket("PROJ-1")], 0);
        let (app, _) = update(app, Msg::Back);
        assert!(app.quit);
    }

    #[test]
    fn back_on_board_with_run_detail_open_closes_it_without_quitting() {
        let app = App {
            show_run_detail: true,
            run_detail: Some(run_detail(1)),
            ..board_with(vec![ticket("PROJ-1")], 0)
        };
        let (app, _) = update(app, Msg::Back);
        assert!(!app.show_run_detail);
        assert_eq!(app.run_detail, None);
        assert!(!app.quit);
    }

    #[test]
    fn back_on_board_closes_overlay_then_quits_on_next_back() {
        let app = App {
            show_run_detail: true,
            ..board_with(vec![ticket("PROJ-1")], 0)
        };
        let (app, _) = update(app, Msg::Back);
        assert!(!app.quit);
        let (app, _) = update(app, Msg::Back);
        assert!(app.quit);
    }

    #[test]
    fn up_and_down_on_board_scroll_run_detail_when_open() {
        let app = App {
            show_run_detail: true,
            run_detail_scroll: 2,
            ..board_with(vec![ticket("PROJ-1")], 0)
        };
        let (app, _) = update(app, Msg::Down);
        assert_eq!(app.run_detail_scroll, 3);
        assert_eq!(app.selected_row, 0);
        let (app, _) = update(app, Msg::Up);
        assert_eq!(app.run_detail_scroll, 2);
        assert_eq!(app.selected_row, 0);
    }

    #[test]
    fn up_on_detail_scrolls_up_clamped_at_zero() {
        let app = App {
            screen: Screen::Detail,
            detail_scroll: 0,
            ..App::new()
        };
        let (app, _) = update(app, Msg::Up);
        assert_eq!(app.detail_scroll, 0);
    }

    #[test]
    fn down_on_detail_scrolls_down() {
        let app = App {
            screen: Screen::Detail,
            detail_scroll: 2,
            ..App::new()
        };
        let (app, _) = update(app, Msg::Down);
        assert_eq!(app.detail_scroll, 3);
    }

    #[test]
    fn up_and_down_on_transition_menu_move_selection_clamped() {
        let app = App {
            screen: Screen::TransitionMenu,
            transitions: vec![transition("11", "A"), transition("21", "B")],
            transition_selected: 0,
            ..App::new()
        };
        let (app, _) = update(app, Msg::Up);
        assert_eq!(app.transition_selected, 0);
        let (app, _) = update(app, Msg::Down);
        assert_eq!(app.transition_selected, 1);
        let (app, _) = update(app, Msg::Down);
        assert_eq!(app.transition_selected, 1);
    }

    #[test]
    fn group_into_columns_orders_by_category_then_name() {
        struct Case {
            name: &'static str,
            tickets: Vec<TicketSummary>,
            expected: Vec<(&'static str, Vec<&'static str>)>,
        }

        let cases = vec![
            Case {
                name: "empty input produces no columns",
                tickets: vec![],
                expected: vec![],
            },
            Case {
                name: "single status produces a single column",
                tickets: vec![ticket_with("PROJ-1", "To Do", "new")],
                expected: vec![("To Do", vec!["PROJ-1"])],
            },
            Case {
                name: "categories ordered new < indeterminate < done",
                tickets: vec![
                    ticket_with("PROJ-1", "Done", "done"),
                    ticket_with("PROJ-2", "In Progress", "indeterminate"),
                    ticket_with("PROJ-3", "To Do", "new"),
                ],
                expected: vec![
                    ("To Do", vec!["PROJ-3"]),
                    ("In Progress", vec!["PROJ-2"]),
                    ("Done", vec!["PROJ-1"]),
                ],
            },
            Case {
                name: "same category sorts alphabetically by status name",
                tickets: vec![
                    ticket_with("PROJ-1", "In Review", "indeterminate"),
                    ticket_with("PROJ-2", "In Progress", "indeterminate"),
                ],
                expected: vec![
                    ("In Progress", vec!["PROJ-2"]),
                    ("In Review", vec!["PROJ-1"]),
                ],
            },
            Case {
                name: "unknown category sorts after done",
                tickets: vec![
                    ticket_with("PROJ-1", "Done", "done"),
                    ticket_with("PROJ-2", "Weird", "some-unknown-category"),
                ],
                expected: vec![("Done", vec!["PROJ-1"]), ("Weird", vec!["PROJ-2"])],
            },
            Case {
                name: "ticket order within a column is preserved",
                tickets: vec![
                    ticket_with("PROJ-1", "To Do", "new"),
                    ticket_with("PROJ-2", "To Do", "new"),
                    ticket_with("PROJ-3", "To Do", "new"),
                ],
                expected: vec![("To Do", vec!["PROJ-1", "PROJ-2", "PROJ-3"])],
            },
        ];

        for case in cases {
            let columns = group_into_columns(case.tickets, &[]);
            let actual: Vec<(&str, Vec<&str>)> = columns
                .iter()
                .map(|c| {
                    (
                        c.title.as_str(),
                        c.tickets.iter().map(|t| t.key.as_str()).collect(),
                    )
                })
                .collect();
            assert_eq!(actual, case.expected, "case: {}", case.name);
        }
    }

    #[test]
    fn group_into_columns_empty_order_is_unchanged() {
        let tickets = vec![
            ticket_with("PROJ-1", "Code Review", "indeterminate"),
            ticket_with("PROJ-2", "In Progress", "indeterminate"),
        ];
        let columns = group_into_columns(tickets, &[]);
        let titles: Vec<&str> = columns.iter().map(|c| c.title.as_str()).collect();
        // Same category, so falls back to alphabetical: Code Review < In Progress.
        assert_eq!(titles, vec!["Code Review", "In Progress"]);
    }

    #[test]
    fn group_into_columns_respects_configured_order() {
        let tickets = vec![
            ticket_with("PROJ-1", "Code Review", "indeterminate"),
            ticket_with("PROJ-2", "In Progress", "indeterminate"),
            ticket_with("PROJ-3", "To Do", "new"),
        ];
        let order = vec!["To Do".to_string(), "In Progress".to_string()];
        let columns = group_into_columns(tickets, &order);
        let titles: Vec<&str> = columns.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(titles, vec!["To Do", "In Progress", "Code Review"]);
    }

    #[test]
    fn group_into_columns_unlisted_columns_keep_category_then_name_order() {
        let tickets = vec![
            ticket_with("PROJ-1", "Backlog", "new"),
            ticket_with("PROJ-2", "Done", "done"),
            ticket_with("PROJ-3", "Code Review", "indeterminate"),
            ticket_with("PROJ-4", "In Progress", "indeterminate"),
        ];
        // Only "In Progress" is listed; everything else keeps the default
        // category-then-name ordering and sorts after it.
        let order = vec!["In Progress".to_string()];
        let columns = group_into_columns(tickets, &order);
        let titles: Vec<&str> = columns.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(
            titles,
            vec!["In Progress", "Backlog", "Code Review", "Done"]
        );
    }

    #[test]
    fn group_into_columns_matches_configured_order_case_insensitively() {
        let tickets = vec![
            ticket_with("PROJ-1", "Code Review", "indeterminate"),
            ticket_with("PROJ-2", "In Progress", "indeterminate"),
        ];
        let order = vec!["in progress".to_string()];
        let columns = group_into_columns(tickets, &order);
        let titles: Vec<&str> = columns.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(titles, vec!["In Progress", "Code Review"]);
    }

    fn jira_user(account_id: &str, display_name: &str) -> JiraUser {
        JiraUser {
            account_id: account_id.to_string(),
            display_name: display_name.to_string(),
        }
    }

    #[test]
    fn open_filter_picker_shows_it_and_fetches_users_when_uncached() {
        let app = App::new();
        let (app, cmds) = update(app, Msg::OpenFilterPicker);
        assert!(app.show_filter_picker);
        assert_eq!(
            cmds,
            vec![Cmd::FetchAssignableUsers {
                project: String::new()
            }]
        );
    }

    #[test]
    fn open_filter_picker_does_not_refetch_when_users_already_cached() {
        let app = App {
            assignable_users: Some(vec![jira_user("acct-1", "Jane Doe")]),
            ..App::new()
        };
        let (app, cmds) = update(app, Msg::OpenFilterPicker);
        assert!(app.show_filter_picker);
        assert!(cmds.is_empty());
    }

    #[test]
    fn open_filter_picker_highlights_the_active_filter() {
        let app = App {
            filter: AssigneeFilter::Everyone,
            ..App::new()
        };
        let (app, _) = update(app, Msg::OpenFilterPicker);
        assert_eq!(app.filter_picker_selected, 2);
    }

    #[test]
    fn open_filter_picker_highlights_active_user_filter_when_cached() {
        let app = App {
            filter: AssigneeFilter::User(jira_user("acct-2", "John Roe")),
            assignable_users: Some(vec![
                jira_user("acct-1", "Jane Doe"),
                jira_user("acct-2", "John Roe"),
            ]),
            ..App::new()
        };
        let (app, _) = update(app, Msg::OpenFilterPicker);
        assert_eq!(app.filter_picker_selected, 4);
    }

    #[test]
    fn filter_picker_up_and_down_navigate_and_clamp() {
        let app = App {
            show_filter_picker: true,
            filter_picker_selected: 0,
            assignable_users: Some(vec![jira_user("acct-1", "Jane Doe")]),
            ..App::new()
        };
        // 4 options: Me, Unassigned, Everyone, Jane Doe.
        let (app, _) = update(app, Msg::FilterPickerUp);
        assert_eq!(app.filter_picker_selected, 0);
        let (app, _) = update(app, Msg::FilterPickerDown);
        assert_eq!(app.filter_picker_selected, 1);
        let (app, _) = update(app, Msg::FilterPickerDown);
        let (app, _) = update(app, Msg::FilterPickerDown);
        assert_eq!(app.filter_picker_selected, 3);
        let (app, _) = update(app, Msg::FilterPickerDown);
        assert_eq!(app.filter_picker_selected, 3);
        let (app, _) = update(app, Msg::FilterPickerUp);
        assert_eq!(app.filter_picker_selected, 2);
    }

    #[test]
    fn filter_picker_select_unassigned_applies_filter_and_fetches_scoped_jql() {
        let app = App {
            show_filter_picker: true,
            filter_picker_selected: 1,
            project_key: "PROJ".to_string(),
            ..App::new()
        };
        let (app, cmds) = update(app, Msg::FilterPickerSelect);
        assert!(!app.show_filter_picker);
        assert_eq!(app.filter, AssigneeFilter::Unassigned);
        assert_eq!(
            cmds,
            vec![Cmd::FetchTickets {
                query: TicketQuery::Unassigned {
                    project_key: "PROJ".to_string()
                }
            }]
        );
    }

    #[test]
    fn filter_picker_select_everyone_applies_filter_and_fetches_scoped_jql() {
        let app = App {
            show_filter_picker: true,
            filter_picker_selected: 2,
            project_key: "PROJ".to_string(),
            ..App::new()
        };
        let (app, cmds) = update(app, Msg::FilterPickerSelect);
        assert_eq!(app.filter, AssigneeFilter::Everyone);
        assert_eq!(
            cmds,
            vec![Cmd::FetchTickets {
                query: TicketQuery::Everyone {
                    project_key: "PROJ".to_string()
                }
            }]
        );
    }

    #[test]
    fn filter_picker_select_specific_user_applies_filter_and_fetches_scoped_jql() {
        let app = App {
            show_filter_picker: true,
            filter_picker_selected: 3,
            project_key: "PROJ".to_string(),
            assignable_users: Some(vec![jira_user("acct-1", "Jane Doe")]),
            ..App::new()
        };
        let (app, cmds) = update(app, Msg::FilterPickerSelect);
        assert_eq!(
            app.filter,
            AssigneeFilter::User(jira_user("acct-1", "Jane Doe"))
        );
        assert_eq!(
            cmds,
            vec![Cmd::FetchTickets {
                query: TicketQuery::Assignee {
                    project_key: "PROJ".to_string(),
                    account_id: "acct-1".to_string()
                }
            }]
        );
    }

    #[test]
    fn filter_picker_select_me_ignores_project_key() {
        let app = App {
            show_filter_picker: true,
            filter_picker_selected: 0,
            project_key: "PROJ".to_string(),
            filter: AssigneeFilter::Everyone,
            ..App::new()
        };
        let (app, cmds) = update(app, Msg::FilterPickerSelect);
        assert_eq!(app.filter, AssigneeFilter::Me);
        assert_eq!(
            cmds,
            vec![Cmd::FetchTickets {
                query: TicketQuery::MyOpen
            }]
        );
    }

    #[test]
    fn filter_picker_select_out_of_range_is_a_noop() {
        let app = App {
            show_filter_picker: true,
            filter_picker_selected: 10,
            filter: AssigneeFilter::Me,
            ..App::new()
        };
        let (app, cmds) = update(app, Msg::FilterPickerSelect);
        assert!(app.show_filter_picker);
        assert_eq!(app.filter, AssigneeFilter::Me);
        assert!(cmds.is_empty());
    }

    #[test]
    fn filter_picker_close_leaves_filter_unchanged() {
        let app = App {
            show_filter_picker: true,
            filter: AssigneeFilter::Me,
            ..App::new()
        };
        let (app, cmds) = update(app, Msg::FilterPickerClose);
        assert!(!app.show_filter_picker);
        assert_eq!(app.filter, AssigneeFilter::Me);
        assert!(cmds.is_empty());
    }

    #[test]
    fn assignable_users_loaded_caches_users_and_clears_error() {
        let app = App {
            filter_picker_error: Some("boom".to_string()),
            ..App::new()
        };
        let (app, cmds) = update(
            app,
            Msg::AssignableUsersLoaded(vec![jira_user("acct-1", "Jane Doe")]),
        );
        assert_eq!(
            app.assignable_users,
            Some(vec![jira_user("acct-1", "Jane Doe")])
        );
        assert_eq!(app.filter_picker_error, None);
        assert!(cmds.is_empty());
    }

    #[test]
    fn assignable_users_failed_sets_picker_error_without_touching_cache() {
        let app = App::new();
        let (app, cmds) = update(app, Msg::AssignableUsersFailed("boom".to_string()));
        assert_eq!(app.filter_picker_error, Some("boom".to_string()));
        assert_eq!(app.assignable_users, None);
        assert!(cmds.is_empty());
    }

    #[test]
    fn open_assign_picker_shows_it_and_fetches_users_when_uncached() {
        let app = board_with(vec![ticket("PROJ-1")], 0);
        let (app, cmds) = update(app, Msg::OpenAssignPicker);
        assert!(app.show_assign_picker);
        assert_eq!(app.assign_picker_key, Some("PROJ-1".to_string()));
        assert_eq!(
            cmds,
            vec![Cmd::FetchAssignableUsers {
                project: String::new()
            }]
        );
    }

    #[test]
    fn open_assign_picker_does_not_refetch_when_users_already_cached() {
        let app = App {
            assignable_users: Some(vec![jira_user("acct-1", "Jane Doe")]),
            ..board_with(vec![ticket("PROJ-1")], 0)
        };
        let (app, cmds) = update(app, Msg::OpenAssignPicker);
        assert!(app.show_assign_picker);
        assert!(cmds.is_empty());
    }

    #[test]
    fn open_assign_picker_is_a_noop_on_an_empty_board() {
        let app = board_with(vec![], 0);
        let (app, cmds) = update(app, Msg::OpenAssignPicker);
        assert!(!app.show_assign_picker);
        assert_eq!(app.assign_picker_key, None);
        assert!(cmds.is_empty());
    }

    #[test]
    fn assign_picker_up_and_down_navigate_and_clamp() {
        let app = App {
            show_assign_picker: true,
            assign_picker_selected: 0,
            assignable_users: Some(vec![jira_user("acct-1", "Jane Doe")]),
            ..board_with(vec![ticket("PROJ-1")], 0)
        };
        // 3 options: Me, Unassign, Jane Doe.
        let (app, _) = update(app, Msg::AssignPickerUp);
        assert_eq!(app.assign_picker_selected, 0);
        let (app, _) = update(app, Msg::AssignPickerDown);
        assert_eq!(app.assign_picker_selected, 1);
        let (app, _) = update(app, Msg::AssignPickerDown);
        assert_eq!(app.assign_picker_selected, 2);
        let (app, _) = update(app, Msg::AssignPickerDown);
        assert_eq!(app.assign_picker_selected, 2);
        let (app, _) = update(app, Msg::AssignPickerUp);
        assert_eq!(app.assign_picker_selected, 1);
    }

    #[test]
    fn assign_picker_select_me_emits_assign_ticket_and_closes_picker() {
        let app = App {
            show_assign_picker: true,
            assign_picker_key: Some("PROJ-1".to_string()),
            assign_picker_selected: 0,
            ..board_with(vec![ticket("PROJ-1")], 0)
        };
        let (app, cmds) = update(app, Msg::AssignPickerSelect);
        assert!(!app.show_assign_picker);
        assert_eq!(app.assign_picker_key, None);
        assert_eq!(
            cmds,
            vec![Cmd::AssignTicket {
                key: "PROJ-1".to_string(),
                choice: AssignChoice::Me,
            }]
        );
    }

    #[test]
    fn assign_picker_select_unassign_emits_assign_ticket() {
        let app = App {
            show_assign_picker: true,
            assign_picker_key: Some("PROJ-1".to_string()),
            assign_picker_selected: 1,
            ..board_with(vec![ticket("PROJ-1")], 0)
        };
        let (app, cmds) = update(app, Msg::AssignPickerSelect);
        assert!(!app.show_assign_picker);
        assert_eq!(
            cmds,
            vec![Cmd::AssignTicket {
                key: "PROJ-1".to_string(),
                choice: AssignChoice::Unassign,
            }]
        );
    }

    #[test]
    fn assign_picker_select_user_emits_assign_ticket() {
        let app = App {
            show_assign_picker: true,
            assign_picker_key: Some("PROJ-1".to_string()),
            assign_picker_selected: 2,
            assignable_users: Some(vec![jira_user("acct-1", "Jane Doe")]),
            ..board_with(vec![ticket("PROJ-1")], 0)
        };
        let (app, cmds) = update(app, Msg::AssignPickerSelect);
        assert!(!app.show_assign_picker);
        assert_eq!(
            cmds,
            vec![Cmd::AssignTicket {
                key: "PROJ-1".to_string(),
                choice: AssignChoice::User(jira_user("acct-1", "Jane Doe")),
            }]
        );
    }

    #[test]
    fn assign_picker_select_out_of_range_is_a_noop() {
        let app = App {
            show_assign_picker: true,
            assign_picker_key: Some("PROJ-1".to_string()),
            assign_picker_selected: 10,
            ..board_with(vec![ticket("PROJ-1")], 0)
        };
        let (app, cmds) = update(app, Msg::AssignPickerSelect);
        assert!(app.show_assign_picker);
        assert_eq!(app.assign_picker_key, Some("PROJ-1".to_string()));
        assert!(cmds.is_empty());
    }

    #[test]
    fn assign_applied_updates_card_assignee_and_status_line() {
        let app = board_with(vec![ticket("PROJ-1")], 0);
        let (app, cmds) = update(
            app,
            Msg::AssignApplied {
                key: "PROJ-1".to_string(),
                assignee: Some("Jane Doe".to_string()),
            },
        );
        assert_eq!(
            app.selected_ticket().unwrap().assignee,
            Some("Jane Doe".to_string())
        );
        assert_eq!(app.status_line, "PROJ-1 -> assigned to Jane Doe");
        assert_eq!(
            cmds,
            vec![Cmd::FetchTickets {
                query: TicketQuery::MyOpen
            }]
        );
    }

    #[test]
    fn assign_applied_with_none_unassigns_card_and_sets_status_line() {
        let app = board_with(vec![ticket("PROJ-1")], 0);
        let (app, cmds) = update(
            app,
            Msg::AssignApplied {
                key: "PROJ-1".to_string(),
                assignee: None,
            },
        );
        assert_eq!(app.selected_ticket().unwrap().assignee, None);
        assert_eq!(app.status_line, "PROJ-1 -> unassigned");
        assert_eq!(
            cmds,
            vec![Cmd::FetchTickets {
                query: TicketQuery::MyOpen
            }]
        );
    }

    #[test]
    fn assign_applied_refetches_under_the_active_non_me_filter() {
        let app = App {
            filter: AssigneeFilter::Everyone,
            project_key: "PROJ".to_string(),
            ..board_with(vec![ticket("PROJ-1")], 0)
        };
        let (app, cmds) = update(
            app,
            Msg::AssignApplied {
                key: "PROJ-1".to_string(),
                assignee: Some("Jane Doe".to_string()),
            },
        );
        assert_eq!(app.status_line, "PROJ-1 -> assigned to Jane Doe");
        assert_eq!(
            cmds,
            vec![Cmd::FetchTickets {
                query: TicketQuery::Everyone {
                    project_key: "PROJ".to_string()
                }
            }]
        );
    }

    #[test]
    fn assign_applied_then_refetch_missing_the_assigned_card_does_not_panic() {
        let app = board_with(vec![ticket("PROJ-1"), ticket("PROJ-2")], 0);
        let (app, cmds) = update(
            app,
            Msg::AssignApplied {
                key: "PROJ-1".to_string(),
                assignee: Some("Jane Doe".to_string()),
            },
        );
        assert_eq!(
            cmds,
            vec![Cmd::FetchTickets {
                query: TicketQuery::MyOpen
            }]
        );
        // Simulate the refetch resolving under the still-active `Me` filter:
        // PROJ-1 was assigned away, so it no longer matches and drops out of
        // the result set. Reselecting must not panic and should land on
        // whatever is left.
        let (app, _) = update(app, Msg::TicketsLoaded(vec![ticket("PROJ-2")]));
        let remaining: Vec<&str> = app
            .columns
            .iter()
            .flat_map(|c| c.tickets.iter().map(|t| t.key.as_str()))
            .collect();
        assert_eq!(remaining, vec!["PROJ-2"]);
        assert_eq!(app.selected_ticket().unwrap().key, "PROJ-2");
    }

    #[test]
    fn assign_failed_sets_status_line() {
        let app = App::new();
        let (app, cmds) = update(app, Msg::AssignFailed("boom".to_string()));
        assert_eq!(app.status_line, "boom");
        assert!(cmds.is_empty());
    }

    #[test]
    fn assignable_users_loaded_clears_assign_picker_error_and_clamps_selection() {
        let app = App {
            assign_picker_error: Some("boom".to_string()),
            assign_picker_selected: 10,
            ..App::new()
        };
        let (app, _) = update(
            app,
            Msg::AssignableUsersLoaded(vec![jira_user("acct-1", "Jane Doe")]),
        );
        assert_eq!(app.assign_picker_error, None);
        // 3 options: Me, Unassign, Jane Doe.
        assert_eq!(app.assign_picker_selected, 2);
    }

    #[test]
    fn assignable_users_failed_sets_assign_picker_error() {
        let app = App::new();
        let (app, _) = update(app, Msg::AssignableUsersFailed("boom".to_string()));
        assert_eq!(app.assign_picker_error, Some("boom".to_string()));
    }

    #[test]
    fn filter_options_lists_me_unassigned_everyone_then_cached_users() {
        let app = App {
            assignable_users: Some(vec![
                jira_user("acct-1", "Jane Doe"),
                jira_user("acct-2", "John Roe"),
            ]),
            ..App::new()
        };
        assert_eq!(
            app.filter_options(),
            vec![
                AssigneeFilter::Me,
                AssigneeFilter::Unassigned,
                AssigneeFilter::Everyone,
                AssigneeFilter::User(jira_user("acct-1", "Jane Doe")),
                AssigneeFilter::User(jira_user("acct-2", "John Roe")),
            ]
        );
    }

    #[test]
    fn filter_options_without_cached_users_lists_only_the_three_builtins() {
        let app = App::new();
        assert_eq!(
            app.filter_options(),
            vec![
                AssigneeFilter::Me,
                AssigneeFilter::Unassigned,
                AssigneeFilter::Everyone,
            ]
        );
    }

    fn rank_app(keys: &[&str], selected: usize) -> App {
        App {
            screen: Screen::Rank,
            project_key: "PROJ".to_string(),
            rank_tickets: keys.iter().map(|k| ticket(k)).collect(),
            rank_selected: selected,
            ..App::new()
        }
    }

    #[test]
    fn open_rank_switches_screen_resets_state_and_fetches_ranked_jql() {
        let app = App {
            project_key: "PROJ".to_string(),
            ..board_with(vec![ticket("PROJ-1")], 0)
        };
        let (app, cmds) = update(app, Msg::OpenRank);
        assert_eq!(app.screen, Screen::Rank);
        assert_eq!(app.rank_selected, 0);
        assert!(app.rank_grab_origin.is_none());
        assert_eq!(
            cmds,
            vec![Cmd::FetchRankTickets {
                query: TicketQuery::Ranked {
                    project_key: "PROJ".to_string()
                }
            }]
        );
    }

    #[test]
    fn refresh_on_rank_screen_fetches_ranked_jql() {
        let app = rank_app(&["PROJ-1"], 0);
        let (app, cmds) = update(app, Msg::Refresh);
        assert_eq!(app.status_line, "Refreshing...");
        assert_eq!(
            cmds,
            vec![Cmd::FetchRankTickets {
                query: TicketQuery::Ranked {
                    project_key: "PROJ".to_string()
                }
            }]
        );
    }

    #[test]
    fn refresh_on_board_screen_still_fetches_board_jql() {
        // Regression guard: adding the rank branch to Refresh must not change
        // the board's existing behavior.
        let app = App::new();
        let (_app, cmds) = update(app, Msg::Refresh);
        assert_eq!(
            cmds,
            vec![Cmd::FetchTickets {
                query: TicketQuery::MyOpen
            }]
        );
    }

    #[test]
    fn open_in_browser_on_rank_screen_uses_rank_selection() {
        let app = rank_app(&["PROJ-1", "PROJ-2"], 1);
        let (_, cmds) = update(app, Msg::OpenInBrowser);
        assert_eq!(
            cmds,
            vec![Cmd::OpenUrl(
                "https://example.atlassian.net/browse/PROJ-2".to_string()
            )]
        );
    }

    #[test]
    fn open_in_browser_on_rank_screen_with_empty_list_emits_nothing() {
        let app = rank_app(&[], 0);
        let (_, cmds) = update(app, Msg::OpenInBrowser);
        assert!(cmds.is_empty());
    }

    #[test]
    fn rank_tickets_loaded_replaces_list_and_clamps_selection() {
        let app = rank_app(&["PROJ-1", "PROJ-2", "PROJ-3"], 2);
        let (app, cmds) = update(app, Msg::RankTicketsLoaded(vec![ticket("PROJ-9")]));
        assert_eq!(app.rank_tickets, vec![ticket("PROJ-9")]);
        assert_eq!(app.rank_selected, 0);
        assert!(cmds.is_empty());
    }

    #[test]
    fn rank_tickets_loaded_preserves_selection_by_key_when_still_present() {
        let app = rank_app(&["PROJ-1", "PROJ-2"], 1);
        let (app, _) = update(
            app,
            Msg::RankTicketsLoaded(vec![ticket("PROJ-0"), ticket("PROJ-2"), ticket("PROJ-3")]),
        );
        assert_eq!(app.rank_selected_ticket().unwrap().key, "PROJ-2");
    }

    #[test]
    fn rank_tickets_loaded_clears_any_in_progress_grab() {
        let app = rank_app(&["PROJ-1", "PROJ-2"], 0);
        let (app, _) = update(app, Msg::RankGrabToggle);
        assert!(app.is_rank_grabbed());
        let (app, _) = update(app, Msg::RankTicketsLoaded(vec![ticket("PROJ-1")]));
        assert!(!app.is_rank_grabbed());
    }

    #[test]
    fn rank_tickets_failed_sets_status_line() {
        let app = rank_app(&["PROJ-1"], 0);
        let (app, cmds) = update(app, Msg::RankTicketsFailed("boom".to_string()));
        assert_eq!(app.status_line, "boom");
        assert!(cmds.is_empty());
    }

    #[test]
    fn rank_grab_toggle_on_empty_list_is_a_noop() {
        let app = rank_app(&[], 0);
        let (app, cmds) = update(app, Msg::RankGrabToggle);
        assert!(!app.is_rank_grabbed());
        assert!(cmds.is_empty());
    }

    #[test]
    fn rank_grab_toggle_grabs_the_highlighted_ticket() {
        let app = rank_app(&["PROJ-1", "PROJ-2"], 1);
        let (app, cmds) = update(app, Msg::RankGrabToggle);
        assert_eq!(app.rank_grab_origin, Some(1));
        assert_eq!(
            app.rank_snapshot,
            Some(vec![ticket("PROJ-1"), ticket("PROJ-2")])
        );
        assert!(cmds.is_empty());
    }

    #[test]
    fn dropping_with_no_next_and_no_prev_is_defensive_and_does_not_panic() {
        // This state is unreachable through normal grab/move/drop flow (a
        // single-item list can never move away from its origin, so the
        // `rank_selected == origin` no-op catches it first) but the drop
        // branch's index arithmetic must stay panic-safe even if that
        // invariant is ever broken by a future change to clamping. Construct
        // the contrived state directly to exercise it.
        let app = App {
            screen: Screen::Rank,
            rank_tickets: vec![ticket("PROJ-1")],
            rank_selected: 0,
            rank_grab_origin: Some(1),
            rank_snapshot: Some(vec![ticket("PROJ-1")]),
            ..App::new()
        };
        let (app, cmds) = update(app, Msg::RankGrabToggle);
        assert!(!app.is_rank_grabbed());
        assert!(cmds.is_empty());
    }

    #[test]
    fn dropping_without_moving_emits_nothing() {
        let app = rank_app(&["PROJ-1", "PROJ-2"], 0);
        let (app, _) = update(app, Msg::RankGrabToggle); // grab
        let (app, cmds) = update(app, Msg::RankGrabToggle); // drop, unmoved
        assert!(!app.is_rank_grabbed());
        assert!(cmds.is_empty());
    }

    #[test]
    fn dropping_mid_list_emits_before_next() {
        let app = rank_app(&["PROJ-1", "PROJ-2", "PROJ-3"], 0);
        let (app, _) = update(app, Msg::RankGrabToggle); // grab PROJ-1
        let (app, _) = update(app, Msg::Down); // swap with PROJ-2: order [2, 1, 3], selected=1
        let (app, cmds) = update(app, Msg::RankGrabToggle); // drop at index 1
        assert!(!app.is_rank_grabbed());
        assert_eq!(
            app.rank_tickets
                .iter()
                .map(|t| t.key.as_str())
                .collect::<Vec<_>>(),
            vec!["PROJ-2", "PROJ-1", "PROJ-3"]
        );
        assert_eq!(
            cmds,
            vec![Cmd::RankTicket {
                key: "PROJ-1".to_string(),
                anchor: RankAnchor::Before("PROJ-3".to_string())
            }]
        );
    }

    #[test]
    fn dropping_at_bottom_emits_after_prev() {
        let app = rank_app(&["PROJ-1", "PROJ-2", "PROJ-3"], 0);
        let (app, _) = update(app, Msg::RankGrabToggle); // grab PROJ-1
        let (app, _) = update(app, Msg::Down); // [2,1,3] selected=1
        let (app, _) = update(app, Msg::Down); // [2,3,1] selected=2
        let (app, cmds) = update(app, Msg::RankGrabToggle); // drop at bottom
        assert_eq!(
            app.rank_tickets
                .iter()
                .map(|t| t.key.as_str())
                .collect::<Vec<_>>(),
            vec!["PROJ-2", "PROJ-3", "PROJ-1"]
        );
        assert_eq!(
            cmds,
            vec![Cmd::RankTicket {
                key: "PROJ-1".to_string(),
                anchor: RankAnchor::After("PROJ-3".to_string())
            }]
        );
    }

    #[test]
    fn dropping_at_top_emits_before_old_first() {
        let app = rank_app(&["PROJ-1", "PROJ-2", "PROJ-3"], 2);
        let (app, _) = update(app, Msg::RankGrabToggle); // grab PROJ-3
        let (app, _) = update(app, Msg::Up); // [1,3,2] selected=1
        let (app, _) = update(app, Msg::Up); // [3,1,2] selected=0
        let (app, cmds) = update(app, Msg::RankGrabToggle); // drop at top
        assert_eq!(
            app.rank_tickets
                .iter()
                .map(|t| t.key.as_str())
                .collect::<Vec<_>>(),
            vec!["PROJ-3", "PROJ-1", "PROJ-2"]
        );
        assert_eq!(
            cmds,
            vec![Cmd::RankTicket {
                key: "PROJ-3".to_string(),
                anchor: RankAnchor::Before("PROJ-1".to_string())
            }]
        );
    }

    #[test]
    fn grabbed_move_up_clamps_at_top_as_a_noop() {
        let app = rank_app(&["PROJ-1", "PROJ-2"], 0);
        let (app, _) = update(app, Msg::RankGrabToggle);
        let (app, _) = update(app, Msg::Up);
        assert_eq!(app.rank_selected, 0);
        assert_eq!(
            app.rank_tickets
                .iter()
                .map(|t| t.key.as_str())
                .collect::<Vec<_>>(),
            vec!["PROJ-1", "PROJ-2"]
        );
    }

    #[test]
    fn grabbed_move_down_clamps_at_bottom_as_a_noop() {
        let app = rank_app(&["PROJ-1", "PROJ-2"], 1);
        let (app, _) = update(app, Msg::RankGrabToggle);
        let (app, _) = update(app, Msg::Down);
        assert_eq!(app.rank_selected, 1);
        assert_eq!(
            app.rank_tickets
                .iter()
                .map(|t| t.key.as_str())
                .collect::<Vec<_>>(),
            vec!["PROJ-1", "PROJ-2"]
        );
    }

    #[test]
    fn ungrabbed_up_and_down_only_move_the_cursor() {
        let app = rank_app(&["PROJ-1", "PROJ-2", "PROJ-3"], 0);
        let (app, _) = update(app, Msg::Down);
        assert_eq!(app.rank_selected, 1);
        assert_eq!(
            app.rank_tickets
                .iter()
                .map(|t| t.key.as_str())
                .collect::<Vec<_>>(),
            vec!["PROJ-1", "PROJ-2", "PROJ-3"]
        );
        let (app, _) = update(app, Msg::Up);
        assert_eq!(app.rank_selected, 0);
    }

    #[test]
    fn ungrabbed_up_and_down_are_noops_on_an_empty_list() {
        let app = rank_app(&[], 0);
        let (app, _) = update(app, Msg::Down);
        assert_eq!(app.rank_selected, 0);
        let (app, _) = update(app, Msg::Up);
        assert_eq!(app.rank_selected, 0);
    }

    #[test]
    fn back_while_grabbed_cancels_and_restores_original_order_and_selection() {
        let app = rank_app(&["PROJ-1", "PROJ-2", "PROJ-3"], 0);
        let (app, _) = update(app, Msg::RankGrabToggle); // grab PROJ-1
        let (app, _) = update(app, Msg::Down); // [2,1,3] selected=1
        let (app, _) = update(app, Msg::Down); // [2,3,1] selected=2
        let (app, cmds) = update(app, Msg::Back); // cancel
        assert!(!app.is_rank_grabbed());
        assert_eq!(app.screen, Screen::Rank, "cancel stays on the rank screen");
        assert_eq!(app.rank_selected, 0, "selection restored to its origin");
        assert_eq!(
            app.rank_tickets
                .iter()
                .map(|t| t.key.as_str())
                .collect::<Vec<_>>(),
            vec!["PROJ-1", "PROJ-2", "PROJ-3"],
            "original order restored"
        );
        assert!(cmds.is_empty());
    }

    #[test]
    fn back_while_not_grabbed_returns_to_board_without_quitting() {
        let app = rank_app(&["PROJ-1"], 0);
        let (app, _) = update(app, Msg::Back);
        assert_eq!(app.screen, Screen::Board);
        assert!(!app.quit);
    }

    #[test]
    fn back_never_quits_from_the_rank_screen_grabbed_or_not() {
        // q and Esc both map to Msg::Back; the rank screen must never let
        // either quit the app outright (unlike the board, where Back quits).
        let grabbed = {
            let app = rank_app(&["PROJ-1"], 0);
            let (app, _) = update(app, Msg::RankGrabToggle);
            app
        };
        let (grabbed, _) = update(grabbed, Msg::Back);
        assert!(!grabbed.quit);

        let not_grabbed = rank_app(&["PROJ-1"], 0);
        let (not_grabbed, _) = update(not_grabbed, Msg::Back);
        assert!(!not_grabbed.quit);
    }

    #[test]
    fn rank_applied_sets_status_line_and_keeps_the_reordered_list() {
        let app = rank_app(&["PROJ-2", "PROJ-1", "PROJ-3"], 1);
        let (app, cmds) = update(
            app,
            Msg::RankApplied("Ranked PROJ-1 above PROJ-3".to_string()),
        );
        assert_eq!(app.status_line, "Ranked PROJ-1 above PROJ-3");
        assert_eq!(
            app.rank_tickets
                .iter()
                .map(|t| t.key.as_str())
                .collect::<Vec<_>>(),
            vec!["PROJ-2", "PROJ-1", "PROJ-3"]
        );
        assert!(cmds.is_empty());
    }

    #[test]
    fn rank_failed_sets_status_line_and_refetches_rank_list() {
        let app = rank_app(&["PROJ-1"], 0);
        let (app, cmds) = update(app, Msg::RankFailed("boom".to_string()));
        assert_eq!(app.status_line, "boom");
        assert_eq!(
            cmds,
            vec![Cmd::FetchRankTickets {
                query: TicketQuery::Ranked {
                    project_key: "PROJ".to_string()
                }
            }]
        );
    }

    fn run_card(id: i64, ticket: &str, status: crate::runs::RunStatus) -> RunCard {
        RunCard {
            id,
            ticket: ticket.to_string(),
            lane: "backend".to_string(),
            kind: "lane".to_string(),
            status,
            age_secs: 10,
            heartbeat_age_secs: Some(5),
            last_event_kind: None,
            last_event_age_secs: None,
            awaiting_input: false,
            checklist: None,
        }
    }

    fn runs_app(cards: Vec<RunCard>, col: usize, row: usize) -> App {
        App {
            screen: Screen::Runs,
            runs: cards,
            runs_selected_col: col,
            runs_selected_row: row,
            ..App::new()
        }
    }

    #[test]
    fn tick_is_a_noop_off_the_runs_screen() {
        // `App::new()` defaults to `Screen::Board`, which (unlike when this
        // test was written) now also reacts to `Msg::Tick` -- see
        // `tick_on_board_emits_load_audit_status_every_8th_tick`. Pick a
        // screen that reacts to neither, to keep testing the true off-both
        // case.
        let app = App {
            screen: Screen::Detail,
            ..App::new()
        };
        let (app, cmds) = update(app, Msg::Tick);
        assert_eq!(app.watch_tick, 0);
        assert!(cmds.is_empty());
    }

    #[test]
    fn tick_on_runs_screen_increments_but_only_loads_every_second_tick() {
        let app = App {
            screen: Screen::Runs,
            ..App::new()
        };
        let (app, cmds) = update(app, Msg::Tick);
        assert_eq!(app.watch_tick, 1);
        assert!(cmds.is_empty());

        let (app, cmds) = update(app, Msg::Tick);
        assert_eq!(app.watch_tick, 2);
        assert_eq!(cmds, vec![Cmd::LoadRuns]);
    }

    #[test]
    fn tick_reaps_every_120th_tick() {
        let app = App {
            screen: Screen::Runs,
            watch_tick: 119,
            ..App::new()
        };
        let (app, cmds) = update(app, Msg::Tick);
        assert_eq!(app.watch_tick, 120);
        assert_eq!(cmds, vec![Cmd::LoadRuns, Cmd::ReapRuns]);
    }

    #[test]
    fn tick_also_loads_run_detail_when_the_detail_window_is_open() {
        let app = App {
            show_run_detail: true,
            ..runs_app(
                vec![run_card(1, "PROJ-1", crate::runs::RunStatus::Running)],
                1,
                0,
            )
        };
        let (app, _) = update(app, Msg::Tick);
        let (app, cmds) = update(app, Msg::Tick);
        assert_eq!(app.watch_tick, 2);
        assert_eq!(cmds, vec![Cmd::LoadRuns, Cmd::LoadRunDetail { run_id: 1 }]);
    }

    #[test]
    fn runs_loaded_preserves_selection_by_id_across_reload() {
        let app = runs_app(
            vec![
                run_card(1, "PROJ-1", crate::runs::RunStatus::Running),
                run_card(2, "PROJ-2", crate::runs::RunStatus::Running),
            ],
            1,
            1,
        );
        let (app, cmds) = update(
            app,
            Msg::RunsLoaded(vec![
                run_card(2, "PROJ-2", crate::runs::RunStatus::Running),
                run_card(3, "PROJ-3", crate::runs::RunStatus::Running),
            ]),
        );
        assert_eq!(app.selected_run_card().unwrap().id, 2);
        assert!(cmds.is_empty());
    }

    #[test]
    fn runs_loaded_clamps_row_when_selected_id_disappears() {
        let app = runs_app(
            vec![
                run_card(1, "PROJ-1", crate::runs::RunStatus::Running),
                run_card(2, "PROJ-2", crate::runs::RunStatus::Running),
            ],
            1,
            1,
        );
        let (app, _) = update(
            app,
            Msg::RunsLoaded(vec![run_card(3, "PROJ-3", crate::runs::RunStatus::Running)]),
        );
        assert_eq!(app.runs_selected_row, 0);
        assert_eq!(app.selected_run_card().unwrap().id, 3);
    }

    #[test]
    fn runs_loaded_with_empty_column_resets_row_to_zero() {
        let app = runs_app(
            vec![run_card(1, "PROJ-1", crate::runs::RunStatus::Running)],
            1,
            0,
        );
        let (app, _) = update(app, Msg::RunsLoaded(vec![]));
        assert_eq!(app.runs_selected_row, 0);
        assert!(app.selected_run_card().is_none());
    }

    #[test]
    fn runs_failed_sets_status_line() {
        let app = App::new();
        let (app, cmds) = update(app, Msg::RunsFailed("boom".to_string()));
        assert_eq!(app.status_line, "boom");
        assert!(cmds.is_empty());
    }

    #[test]
    fn run_detail_failed_sets_status_line() {
        let app = App::new();
        let (app, cmds) = update(app, Msg::RunDetailFailed("boom".to_string()));
        assert_eq!(app.status_line, "boom");
        assert!(cmds.is_empty());
    }

    fn run_detail(id: i64) -> RunDetail {
        RunDetail {
            id,
            ticket: "PROJ-1".to_string(),
            lane: "backend".to_string(),
            kind: "lane".to_string(),
            status: crate::runs::RunStatus::Running,
            worktree: "/tmp/wt".to_string(),
            branch: None,
            pid: None,
            session_id: None,
            cost_usd: None,
            num_turns: None,
            pr_url: None,
            blocker: None,
            started_at: "2020-01-01T00:00:00.000Z".to_string(),
            ended_at: None,
            events: vec![],
            checklist: None,
            tool_counts: vec![],
            model_usage: None,
            agent_usage: vec![],
        }
    }

    #[test]
    fn run_detail_loaded_sets_detail() {
        let app = App::new();
        let (app, cmds) = update(app, Msg::RunDetailLoaded(Box::new(run_detail(1))));
        assert_eq!(app.run_detail, Some(run_detail(1)));
        assert!(cmds.is_empty());
    }

    #[test]
    fn run_detail_failed_with_nothing_loaded_yet_closes_the_overlay() {
        let app = App {
            show_run_detail: true,
            run_detail: None,
            ..App::new()
        };
        let (app, cmds) = update(app, Msg::RunDetailFailed("boom".to_string()));
        assert_eq!(app.status_line, "boom");
        assert!(!app.show_run_detail);
        assert!(cmds.is_empty());
    }

    #[test]
    fn run_detail_failed_after_a_successful_load_leaves_the_overlay_open() {
        let app = App {
            show_run_detail: true,
            run_detail: Some(run_detail(1)),
            ..App::new()
        };
        let (app, cmds) = update(app, Msg::RunDetailFailed("boom".to_string()));
        assert_eq!(app.status_line, "boom");
        assert!(app.show_run_detail);
        assert_eq!(app.run_detail, Some(run_detail(1)));
        assert!(cmds.is_empty());
    }

    #[test]
    fn runs_reaped_zero_is_a_noop() {
        let app = App::new();
        let (app, cmds) = update(app, Msg::RunsReaped(0));
        assert_eq!(app.status_line, "");
        assert!(cmds.is_empty());
    }

    #[test]
    fn runs_reaped_nonzero_sets_status_line() {
        let app = App::new();
        let (app, _) = update(app, Msg::RunsReaped(2));
        assert_eq!(app.status_line, "Reaped 2 dead run(s)");
    }

    #[test]
    fn h_and_l_move_between_run_columns_and_clamp() {
        let app = runs_app(
            vec![run_card(1, "PROJ-1", crate::runs::RunStatus::Running)],
            1,
            0,
        );
        let (app, _) = update(app, Msg::Right);
        assert_eq!(app.runs_selected_col, 2);
        let (app, _) = update(app, Msg::Left);
        assert_eq!(app.runs_selected_col, 1);
    }

    #[test]
    fn l_clamps_at_the_last_run_column() {
        let app = runs_app(vec![], RUN_COLUMNS.len() - 1, 0);
        let (app, _) = update(app, Msg::Right);
        assert_eq!(app.runs_selected_col, RUN_COLUMNS.len() - 1);
    }

    #[test]
    fn h_clamps_at_the_first_run_column() {
        let app = runs_app(vec![], 0, 0);
        let (app, _) = update(app, Msg::Left);
        assert_eq!(app.runs_selected_col, 0);
    }

    #[test]
    fn moving_columns_clamps_row_into_the_new_columns_bounds() {
        let app = runs_app(
            vec![
                run_card(1, "PROJ-1", crate::runs::RunStatus::Running),
                run_card(2, "PROJ-2", crate::runs::RunStatus::Running),
            ],
            1,
            1,
        );
        // Column 2 (Blocked) is empty; moving into it must clamp the row.
        let (app, _) = update(app, Msg::Right);
        assert_eq!(app.runs_selected_col, 2);
        assert_eq!(app.runs_selected_row, 0);
    }

    #[test]
    fn j_and_k_move_the_row_within_a_run_column_and_clamp() {
        let app = runs_app(
            vec![
                run_card(1, "PROJ-1", crate::runs::RunStatus::Running),
                run_card(2, "PROJ-2", crate::runs::RunStatus::Running),
            ],
            1,
            0,
        );
        let (app, _) = update(app, Msg::Down);
        assert_eq!(app.runs_selected_row, 1);
        let (app, _) = update(app, Msg::Down);
        assert_eq!(app.runs_selected_row, 1);
        let (app, _) = update(app, Msg::Up);
        assert_eq!(app.runs_selected_row, 0);
        let (app, _) = update(app, Msg::Up);
        assert_eq!(app.runs_selected_row, 0);
    }

    #[test]
    fn j_and_k_are_noops_on_an_empty_run_column() {
        let app = runs_app(vec![], 0, 0);
        let (app, _) = update(app, Msg::Down);
        assert_eq!(app.runs_selected_row, 0);
        let (app, _) = update(app, Msg::Up);
        assert_eq!(app.runs_selected_row, 0);
    }

    #[test]
    fn enter_on_runs_screen_opens_detail_and_emits_load_run_detail() {
        let app = runs_app(
            vec![run_card(7, "PROJ-7", crate::runs::RunStatus::Running)],
            1,
            0,
        );
        let (app, cmds) = update(app, Msg::Enter);
        assert!(app.show_run_detail);
        assert_eq!(app.run_detail, None);
        assert_eq!(app.run_detail_scroll, 0);
        assert_eq!(cmds, vec![Cmd::LoadRunDetail { run_id: 7 }]);
    }

    #[test]
    fn enter_on_empty_run_column_is_a_noop() {
        let app = runs_app(vec![], 0, 0);
        let (app, cmds) = update(app, Msg::Enter);
        assert!(!app.show_run_detail);
        assert!(cmds.is_empty());
    }

    #[test]
    fn back_closes_the_detail_window_without_quitting() {
        let app = App {
            show_run_detail: true,
            run_detail: Some(run_detail(1)),
            ..runs_app(
                vec![run_card(1, "PROJ-1", crate::runs::RunStatus::Running)],
                1,
                0,
            )
        };
        let (app, _) = update(app, Msg::Back);
        assert!(!app.show_run_detail);
        assert_eq!(app.run_detail, None);
        assert!(!app.quit);
    }

    #[test]
    fn back_with_no_detail_open_quits() {
        let app = runs_app(vec![], 0, 0);
        let (app, _) = update(app, Msg::Back);
        assert!(app.quit);
    }

    #[test]
    fn j_and_k_scroll_the_detail_window_when_open() {
        let app = App {
            show_run_detail: true,
            run_detail_scroll: 2,
            ..runs_app(vec![], 0, 0)
        };
        let (app, _) = update(app, Msg::Down);
        assert_eq!(app.run_detail_scroll, 3);
        let (app, _) = update(app, Msg::Up);
        assert_eq!(app.run_detail_scroll, 2);
    }

    #[test]
    fn refresh_on_runs_screen_emits_load_runs() {
        let app = runs_app(vec![], 0, 0);
        let (app, cmds) = update(app, Msg::Refresh);
        assert_eq!(app.status_line, "Refreshing...");
        assert_eq!(cmds, vec![Cmd::LoadRuns]);
    }

    #[test]
    fn refresh_on_runs_screen_with_detail_open_also_reloads_detail() {
        let app = App {
            show_run_detail: true,
            ..runs_app(
                vec![run_card(4, "PROJ-4", crate::runs::RunStatus::Running)],
                1,
                0,
            )
        };
        let (_app, cmds) = update(app, Msg::Refresh);
        assert_eq!(cmds, vec![Cmd::LoadRuns, Cmd::LoadRunDetail { run_id: 4 }]);
    }

    // --- audit_indicator ---

    #[test]
    fn audit_indicator_running_and_awaiting_is_waiting() {
        assert_eq!(
            audit_indicator(true, Some((RunStatus::Running, true))),
            Some(AuditIndicator::Waiting)
        );
        // Session existence is irrelevant once there's a running run: the
        // run itself is the signal.
        assert_eq!(
            audit_indicator(false, Some((RunStatus::Running, true))),
            Some(AuditIndicator::Waiting)
        );
    }

    #[test]
    fn audit_indicator_running_not_awaiting_is_running() {
        assert_eq!(
            audit_indicator(true, Some((RunStatus::Running, false))),
            Some(AuditIndicator::Running)
        );
        assert_eq!(
            audit_indicator(false, Some((RunStatus::Running, false))),
            Some(AuditIndicator::Running)
        );
    }

    #[test]
    fn audit_indicator_session_with_no_run_is_starting() {
        assert_eq!(audit_indicator(true, None), Some(AuditIndicator::Starting));
    }

    #[test]
    fn audit_indicator_no_session_and_no_run_is_none() {
        assert_eq!(audit_indicator(false, None), None);
    }

    #[test]
    fn audit_indicator_done_with_live_session_is_done() {
        assert_eq!(
            audit_indicator(true, Some((RunStatus::Done, false))),
            Some(AuditIndicator::Done)
        );
    }

    #[test]
    fn audit_indicator_done_without_session_is_none() {
        assert_eq!(audit_indicator(false, Some((RunStatus::Done, false))), None);
    }

    #[test]
    fn audit_indicator_failed_with_live_session_is_failed() {
        assert_eq!(
            audit_indicator(true, Some((RunStatus::Failed, false))),
            Some(AuditIndicator::Failed)
        );
    }

    #[test]
    fn audit_indicator_failed_without_session_is_none() {
        assert_eq!(
            audit_indicator(false, Some((RunStatus::Failed, false))),
            None
        );
    }

    #[test]
    fn audit_indicator_other_statuses_are_none_regardless_of_session() {
        for status in [RunStatus::Queued, RunStatus::Blocked, RunStatus::Review] {
            assert_eq!(audit_indicator(true, Some((status, false))), None);
            assert_eq!(audit_indicator(false, Some((status, false))), None);
        }
    }

    #[test]
    fn audit_indicator_interrupted_with_live_session_is_interrupted() {
        assert_eq!(
            audit_indicator(true, Some((RunStatus::Interrupted, false))),
            Some(AuditIndicator::Interrupted)
        );
    }

    #[test]
    fn audit_indicator_interrupted_without_session_is_none() {
        assert_eq!(
            audit_indicator(false, Some((RunStatus::Interrupted, false))),
            None
        );
    }

    // --- lane_run_indicator ---

    #[test]
    fn lane_run_indicator_running_and_awaiting_is_waiting() {
        assert_eq!(
            lane_run_indicator(true, Some((RunStatus::Running, true))),
            Some(RunIndicator::Waiting)
        );
        assert_eq!(
            lane_run_indicator(false, Some((RunStatus::Running, true))),
            Some(RunIndicator::Waiting)
        );
    }

    #[test]
    fn lane_run_indicator_blocked_is_waiting() {
        assert_eq!(
            lane_run_indicator(true, Some((RunStatus::Blocked, false))),
            Some(RunIndicator::Waiting)
        );
    }

    #[test]
    fn lane_run_indicator_running_not_awaiting_is_running() {
        assert_eq!(
            lane_run_indicator(true, Some((RunStatus::Running, false))),
            Some(RunIndicator::Running)
        );
        assert_eq!(
            lane_run_indicator(false, Some((RunStatus::Running, false))),
            Some(RunIndicator::Running)
        );
    }

    #[test]
    fn lane_run_indicator_queued_is_running() {
        assert_eq!(
            lane_run_indicator(false, Some((RunStatus::Queued, false))),
            Some(RunIndicator::Running)
        );
    }

    #[test]
    fn lane_run_indicator_review_and_done_are_done() {
        assert_eq!(
            lane_run_indicator(false, Some((RunStatus::Review, false))),
            Some(RunIndicator::Done)
        );
        assert_eq!(
            lane_run_indicator(false, Some((RunStatus::Done, false))),
            Some(RunIndicator::Done)
        );
    }

    #[test]
    fn lane_run_indicator_failed_is_failed() {
        assert_eq!(
            lane_run_indicator(false, Some((RunStatus::Failed, false))),
            Some(RunIndicator::Failed)
        );
    }

    #[test]
    fn lane_run_indicator_interrupted_is_interrupted() {
        assert_eq!(
            lane_run_indicator(false, Some((RunStatus::Interrupted, false))),
            Some(RunIndicator::Interrupted)
        );
    }

    #[test]
    fn lane_run_indicator_pending_with_no_run_is_starting() {
        assert_eq!(lane_run_indicator(true, None), Some(RunIndicator::Starting));
    }

    #[test]
    fn lane_run_indicator_no_pending_and_no_run_is_none() {
        assert_eq!(lane_run_indicator(false, None), None);
    }

    #[test]
    fn lane_run_indicator_prefers_run_row_over_pending() {
        // A run row exists (Done) even though a launch is still marked
        // pending: the row is fresher truth, so the terminal indicator wins
        // rather than Starting.
        assert_eq!(
            lane_run_indicator(true, Some((RunStatus::Done, false))),
            Some(RunIndicator::Done)
        );
    }

    // --- bot_watch_indicator ---

    #[test]
    fn bot_watch_indicator_maps_every_status_to_its_badge() {
        let cases = [
            (RunStatus::Running, Some(BotWatchIndicator::Watching)),
            (RunStatus::Review, Some(BotWatchIndicator::Ready)),
            (RunStatus::Done, Some(BotWatchIndicator::Clean)),
            (RunStatus::Failed, Some(BotWatchIndicator::Failed)),
            (RunStatus::Queued, None),
            (RunStatus::Blocked, None),
        ];
        for (status, expected) in cases {
            assert_eq!(
                bot_watch_indicator(Some(status)),
                expected,
                "unexpected indicator for {status:?}"
            );
        }
    }

    #[test]
    fn bot_watch_indicator_with_no_run_is_none() {
        assert_eq!(bot_watch_indicator(None), None);
    }

    // --- Msg::BotsAction precedence ---

    #[test]
    fn bots_action_with_no_selected_ticket_is_a_noop() {
        let app = board_with(vec![], 0);
        let (_app, cmds) = update(app, Msg::BotsAction);
        assert!(cmds.is_empty());
    }

    #[test]
    fn bots_action_with_live_cleanup_session_attaches() {
        let mut app = board_with(vec![ticket("PROJ-1")], 0);
        app.cleanup_status.insert(
            "PROJ-1".to_string(),
            audit_entry(AuditIndicator::Running, true),
        );
        // A `Ready` watcher must not win over a live cleanup session.
        app.bot_watch_status
            .insert("PROJ-1".to_string(), BotWatchIndicator::Ready);
        let (_app, cmds) = update(app, Msg::BotsAction);
        assert_eq!(
            cmds,
            vec![Cmd::AttachSession {
                session_name: "tm-proj-proj-1".to_string()
            }]
        );
    }

    /// Issue #2 phase 5: `s` attaches to the selected ticket's session
    /// unconditionally — it is the whole ticket's session, not any one
    /// action's, so there is nothing to launch and no liveness to consult.
    #[test]
    fn session_action_attaches_to_the_selected_tickets_session() {
        let app = board_with(vec![ticket("PROJ-1")], 0);

        let (_app, cmds) = update(app, Msg::SessionAction);

        assert_eq!(
            cmds,
            vec![Cmd::AttachSession {
                session_name: "tm-proj-proj-1".to_string()
            }]
        );
    }

    /// No audit/cleanup status is consulted: a ticket whose session holds
    /// only a `work` window (or only a `shell`) is still attachable, and a
    /// session that does not exist reports its own failure from `tmux
    /// attach-session` rather than needing a board-side liveness map.
    #[test]
    fn session_action_does_not_depend_on_any_action_badge() {
        let mut app = board_with(vec![ticket("PROJ-1")], 0);
        app.audit_status.clear();
        app.cleanup_status.clear();

        let (_app, cmds) = update(app, Msg::SessionAction);

        assert_eq!(cmds.len(), 1);
    }

    #[test]
    fn session_action_is_a_no_op_with_no_ticket_selected() {
        let app = App::new();

        let (_app, cmds) = update(app, Msg::SessionAction);

        assert!(cmds.is_empty());
    }

    #[test]
    fn session_attach_result_reports_on_the_status_line() {
        let app = App::new();

        let (app, cmds) = update(app, Msg::SessionAttachResult("detached from x".to_string()));

        assert!(cmds.is_empty());
        assert_eq!(app.status_line, "detached from x");
    }

    #[test]
    fn bots_action_with_ready_watcher_and_no_session_launches_cleanup() {
        let mut app = board_with(vec![ticket("PROJ-1")], 0);
        app.bot_watch_status
            .insert("PROJ-1".to_string(), BotWatchIndicator::Ready);
        let (_app, cmds) = update(app, Msg::BotsAction);
        assert_eq!(
            cmds,
            vec![Cmd::LaunchCleanup {
                key: "PROJ-1".to_string()
            }]
        );
    }

    #[test]
    fn bots_action_with_running_watcher_only_reports_status() {
        let mut app = board_with(vec![ticket("PROJ-1")], 0);
        app.bot_watch_status
            .insert("PROJ-1".to_string(), BotWatchIndicator::Watching);
        let (app, cmds) = update(app, Msg::BotsAction);
        assert!(cmds.is_empty());
        assert_eq!(
            app.status_line,
            "watching PR for PROJ-1 -- bots not done yet"
        );
        assert!(app.pending_bot_watch_launches.is_empty());
    }

    #[test]
    fn bots_action_with_no_watcher_arms_one_and_marks_pending() {
        let app = board_with(vec![ticket("PROJ-1")], 0);
        let (app, cmds) = update(app, Msg::BotsAction);
        assert_eq!(
            cmds,
            vec![Cmd::LaunchBotWatch {
                key: "PROJ-1".to_string()
            }]
        );
        assert!(app.pending_bot_watch_launches.contains("PROJ-1"));
    }

    #[test]
    fn bots_action_with_terminal_watcher_rearms() {
        for indicator in [BotWatchIndicator::Clean, BotWatchIndicator::Failed] {
            let mut app = board_with(vec![ticket("PROJ-1")], 0);
            app.bot_watch_status.insert("PROJ-1".to_string(), indicator);
            let (_app, cmds) = update(app, Msg::BotsAction);
            assert_eq!(
                cmds,
                vec![Cmd::LaunchBotWatch {
                    key: "PROJ-1".to_string()
                }],
                "{indicator:?} should re-arm a watcher"
            );
        }
    }

    #[test]
    fn bots_action_with_a_pending_launch_reports_instead_of_relaunching() {
        let mut app = board_with(vec![ticket("PROJ-1")], 0);
        app.pending_bot_watch_launches.insert("PROJ-1".to_string());
        let (app, cmds) = update(app, Msg::BotsAction);
        assert!(cmds.is_empty());
        assert_eq!(app.status_line, "arming PR watcher for PROJ-1");
    }

    #[test]
    fn bots_action_with_cleanup_entry_but_no_live_session_falls_through() {
        // `Done`/`Failed` with `window_live: false` never reaches the map (see
        // `audit_indicator`), but `Running` without a live session can -- and
        // must not attach to something that isn't there.
        let mut app = board_with(vec![ticket("PROJ-1")], 0);
        app.cleanup_status.insert(
            "PROJ-1".to_string(),
            audit_entry(AuditIndicator::Running, false),
        );
        app.bot_watch_status
            .insert("PROJ-1".to_string(), BotWatchIndicator::Ready);
        let (_app, cmds) = update(app, Msg::BotsAction);
        assert_eq!(
            cmds,
            vec![Cmd::LaunchCleanup {
                key: "PROJ-1".to_string()
            }]
        );
    }

    // --- Msg::BotsActionResult / Msg::BotWatchLaunchResult ---

    #[test]
    fn bots_action_result_sets_status_line() {
        let app = App::new();
        let (app, cmds) = update(
            app,
            Msg::BotsActionResult("launched bugbot cleanup for PROJ-1".to_string()),
        );
        assert_eq!(app.status_line, "launched bugbot cleanup for PROJ-1");
        assert!(cmds.is_empty());
    }

    #[test]
    fn bot_watch_launch_result_ok_removes_pending_and_sets_message() {
        let mut app = App::new();
        app.pending_bot_watch_launches.insert("PROJ-1".to_string());
        let (app, cmds) = update(
            app,
            Msg::BotWatchLaunchResult {
                key: "PROJ-1".to_string(),
                result: Ok(()),
            },
        );
        assert!(app.pending_bot_watch_launches.is_empty());
        assert_eq!(app.status_line, "watching PR for PROJ-1");
        assert!(cmds.is_empty());
    }

    #[test]
    fn bot_watch_launch_result_err_removes_pending_and_sets_message() {
        let mut app = App::new();
        app.pending_bot_watch_launches.insert("PROJ-1".to_string());
        let (app, cmds) = update(
            app,
            Msg::BotWatchLaunchResult {
                key: "PROJ-1".to_string(),
                result: Err("no open pull request found for PROJ-1".to_string()),
            },
        );
        assert!(app.pending_bot_watch_launches.is_empty());
        assert_eq!(
            app.status_line,
            "PR watch failed for PROJ-1: no open pull request found for PROJ-1"
        );
        assert!(cmds.is_empty());
    }

    // --- Msg::BotWatchStatusLoaded / Msg::CleanupStatusLoaded ---

    #[test]
    fn bot_watch_status_loaded_replaces_bot_watch_status() {
        let app = App::new();
        let mut status = HashMap::new();
        status.insert("PROJ-1".to_string(), BotWatchIndicator::Ready);
        let (app, cmds) = update(app, Msg::BotWatchStatusLoaded(status.clone()));
        assert_eq!(app.bot_watch_status, status);
        assert!(cmds.is_empty());
    }

    #[test]
    fn cleanup_status_loaded_replaces_cleanup_status() {
        let app = App::new();
        let mut status = HashMap::new();
        status.insert(
            "PROJ-1".to_string(),
            audit_entry(AuditIndicator::Running, true),
        );
        let (app, cmds) = update(app, Msg::CleanupStatusLoaded(status.clone()));
        assert_eq!(app.cleanup_status, status);
        assert!(cmds.is_empty());
    }

    // --- Msg::AuditAction / Msg::AuditStatusLoaded / Msg::AuditActionResult ---

    fn audit_entry(indicator: AuditIndicator, window_live: bool) -> AuditStatusEntry {
        AuditStatusEntry {
            indicator,
            window_live,
        }
    }

    #[test]
    fn audit_action_with_no_selected_ticket_is_a_noop() {
        let app = board_with(vec![], 0);
        let (_app, cmds) = update(app, Msg::AuditAction);
        assert!(cmds.is_empty());
    }

    #[test]
    fn audit_action_with_no_status_entry_launches() {
        let app = board_with(vec![ticket("PROJ-1")], 0);
        let (_app, cmds) = update(app, Msg::AuditAction);
        assert_eq!(
            cmds,
            vec![Cmd::LaunchAudit {
                key: "PROJ-1".to_string()
            }]
        );
    }

    #[test]
    fn audit_action_with_live_session_attaches() {
        let mut app = board_with(vec![ticket("PROJ-1")], 0);
        app.audit_status.insert(
            "PROJ-1".to_string(),
            audit_entry(AuditIndicator::Running, true),
        );
        let (_app, cmds) = update(app, Msg::AuditAction);
        assert_eq!(
            cmds,
            vec![Cmd::AttachSession {
                session_name: "tm-proj-proj-1".to_string()
            }]
        );
    }

    #[test]
    fn audit_action_with_terminal_status_but_no_session_launches() {
        // Running/Waiting/Done/Failed with `window_live: false` must still
        // launch, not attach: there's nothing live to attach to.
        let mut app = board_with(vec![ticket("PROJ-1")], 0);
        app.audit_status.insert(
            "PROJ-1".to_string(),
            audit_entry(AuditIndicator::Done, false),
        );
        let (_app, cmds) = update(app, Msg::AuditAction);
        assert_eq!(
            cmds,
            vec![Cmd::LaunchAudit {
                key: "PROJ-1".to_string()
            }]
        );
    }

    #[test]
    fn audit_status_loaded_replaces_audit_status() {
        let app = App::new();
        let mut status = HashMap::new();
        status.insert(
            "PROJ-1".to_string(),
            audit_entry(AuditIndicator::Starting, true),
        );
        let (app, cmds) = update(app, Msg::AuditStatusLoaded(status.clone()));
        assert_eq!(app.audit_status, status);
        assert!(cmds.is_empty());
    }

    #[test]
    fn audit_action_result_sets_status_line() {
        let app = App::new();
        let (app, cmds) = update(
            app,
            Msg::AuditActionResult("launched audit for PROJ-1 -- press a to attach".to_string()),
        );
        assert_eq!(
            app.status_line,
            "launched audit for PROJ-1 -- press a to attach"
        );
        assert!(cmds.is_empty());
    }

    // --- Msg::ViewRunAction ---

    #[test]
    fn view_run_action_with_no_selected_ticket_is_a_noop() {
        let app = board_with(vec![], 0);
        let (app, cmds) = update(app, Msg::ViewRunAction);
        assert!(!app.show_run_detail);
        assert!(cmds.is_empty());
    }

    #[test]
    fn view_run_action_opens_overlay_and_emits_load_ticket_run_detail() {
        let app = board_with(vec![ticket("PROJ-1")], 0);
        let (app, cmds) = update(app, Msg::ViewRunAction);
        assert!(app.show_run_detail);
        assert_eq!(app.run_detail, None);
        assert_eq!(app.run_detail_scroll, 0);
        assert_eq!(
            cmds,
            vec![Cmd::LoadTicketRunDetail {
                key: "PROJ-1".to_string()
            }]
        );
    }

    #[test]
    fn view_run_action_off_the_board_screen_is_a_noop() {
        let app = App {
            screen: Screen::Detail,
            ..board_with(vec![ticket("PROJ-1")], 0)
        };
        let (app, cmds) = update(app, Msg::ViewRunAction);
        assert!(!app.show_run_detail);
        assert!(cmds.is_empty());
    }

    // --- Msg::ViewLogsAction / Msg::LogsActionResult ---

    #[test]
    fn view_logs_action_with_no_selected_ticket_is_a_noop() {
        let app = board_with(vec![], 0);
        let (_app, cmds) = update(app, Msg::ViewLogsAction);
        assert!(cmds.is_empty());
    }

    #[test]
    fn view_logs_action_emits_view_logs_for_the_selected_ticket() {
        let app = board_with(vec![ticket("PROJ-1")], 0);
        let (_app, cmds) = update(app, Msg::ViewLogsAction);
        assert_eq!(
            cmds,
            vec![Cmd::ViewLogs {
                key: "PROJ-1".to_string()
            }]
        );
    }

    #[test]
    fn view_logs_action_off_the_board_screen_is_a_noop() {
        let app = App {
            screen: Screen::Detail,
            ..board_with(vec![ticket("PROJ-1")], 0)
        };
        let (_app, cmds) = update(app, Msg::ViewLogsAction);
        assert!(cmds.is_empty());
    }

    #[test]
    fn logs_action_result_sets_status_line() {
        let app = board_with(vec![], 0);
        let (app, cmds) = update(app, Msg::LogsActionResult("no log for PROJ-1".to_string()));
        assert_eq!(app.status_line, "no log for PROJ-1");
        assert!(cmds.is_empty());
    }

    // --- Msg::ViewDiffAction / Msg::DiffActionResult ---

    #[test]
    fn view_diff_action_with_no_selected_ticket_is_a_noop() {
        let app = board_with(vec![], 0);
        let (_app, cmds) = update(app, Msg::ViewDiffAction);
        assert!(cmds.is_empty());
    }

    #[test]
    fn view_diff_action_off_the_board_screen_is_a_noop() {
        let app = App {
            screen: Screen::Detail,
            ..board_with(vec![ticket("PROJ-1")], 0)
        };
        let (_app, cmds) = update(app, Msg::ViewDiffAction);
        assert!(cmds.is_empty());
    }

    #[test]
    fn view_diff_action_with_no_lane_run_sets_status_message_and_no_launch() {
        let app = board_with(vec![ticket("PROJ-1")], 0);
        let (app, cmds) = update(app, Msg::ViewDiffAction);
        assert_eq!(app.status_line, "no lane run for PROJ-1 -- press w first");
        assert!(cmds.is_empty());
    }

    #[test]
    fn view_diff_action_with_starting_lane_run_sets_status_message_and_no_launch() {
        let mut app = board_with(vec![ticket("PROJ-1")], 0);
        app.lane_run_status
            .insert("PROJ-1".to_string(), RunIndicator::Starting);
        let (app, cmds) = update(app, Msg::ViewDiffAction);
        assert_eq!(app.status_line, "lane run for PROJ-1 is still starting");
        assert!(cmds.is_empty());
    }

    #[test]
    fn view_diff_action_with_a_lane_run_emits_view_diff() {
        let mut app = board_with(vec![ticket("PROJ-1")], 0);
        app.lane_run_status
            .insert("PROJ-1".to_string(), RunIndicator::Done);
        let (_app, cmds) = update(app, Msg::ViewDiffAction);
        assert_eq!(
            cmds,
            vec![Cmd::ViewDiff {
                key: "PROJ-1".to_string()
            }]
        );
    }

    #[test]
    fn diff_action_result_sets_status_line() {
        let app = board_with(vec![], 0);
        let (app, cmds) = update(
            app,
            Msg::DiffActionResult("reviewed PROJ-1 in vdiff".to_string()),
        );
        assert_eq!(app.status_line, "reviewed PROJ-1 in vdiff");
        assert!(cmds.is_empty());
    }

    // --- Msg::ReviewFixAction / Msg::ReviewFixLaunchResult ---

    #[test]
    fn review_fix_action_with_no_selected_ticket_is_a_noop() {
        let app = board_with(vec![], 0);
        let (_app, cmds) = update(app, Msg::ReviewFixAction);
        assert!(cmds.is_empty());
    }

    #[test]
    fn review_fix_action_off_the_board_screen_is_a_noop() {
        let app = App {
            screen: Screen::Detail,
            ..board_with(vec![ticket("PROJ-1")], 0)
        };
        let (_app, cmds) = update(app, Msg::ReviewFixAction);
        assert!(cmds.is_empty());
    }

    #[test]
    fn review_fix_action_with_no_lane_run_sets_status_message_and_no_launch() {
        let app = board_with(vec![ticket("PROJ-1")], 0);
        let (app, cmds) = update(app, Msg::ReviewFixAction);
        assert_eq!(app.status_line, "no lane run for PROJ-1 -- press w first");
        assert!(cmds.is_empty());
    }

    #[test]
    fn review_fix_action_with_a_lane_run_emits_launch_review_fix() {
        let mut app = board_with(vec![ticket("PROJ-1")], 0);
        app.lane_run_status
            .insert("PROJ-1".to_string(), RunIndicator::Done);
        let (app, cmds) = update(app, Msg::ReviewFixAction);
        assert_eq!(app.status_line, "dispatching fix pass for PROJ-1");
        assert_eq!(
            cmds,
            vec![Cmd::LaunchReviewFix {
                key: "PROJ-1".to_string()
            }]
        );
    }

    #[test]
    fn review_fix_action_with_a_starting_lane_run_still_launches() {
        // Unlike `Msg::ViewDiffAction`, `Starting` doesn't block `F`: it's a
        // watched-child launch, so it's fine to queue it before the lane
        // run's own preflight finishes -- `tm review fix` fails fast with
        // its own stderr if the worktree still doesn't exist by the time it
        // runs.
        let mut app = board_with(vec![ticket("PROJ-1")], 0);
        app.lane_run_status
            .insert("PROJ-1".to_string(), RunIndicator::Starting);
        let (_app, cmds) = update(app, Msg::ReviewFixAction);
        assert_eq!(
            cmds,
            vec![Cmd::LaunchReviewFix {
                key: "PROJ-1".to_string()
            }]
        );
    }

    #[test]
    fn review_fix_launch_result_ok_sets_status_line() {
        let app = board_with(vec![], 0);
        let (app, cmds) = update(
            app,
            Msg::ReviewFixLaunchResult {
                key: "PROJ-1".to_string(),
                result: Ok(()),
            },
        );
        assert_eq!(app.status_line, "fix pass dispatched for PROJ-1");
        assert!(cmds.is_empty());
    }

    #[test]
    fn review_fix_launch_result_err_sets_status_line_with_stderr() {
        let app = board_with(vec![], 0);
        let (app, cmds) = update(
            app,
            Msg::ReviewFixLaunchResult {
                key: "PROJ-1".to_string(),
                result: Err("no comments captured".to_string()),
            },
        );
        assert_eq!(
            app.status_line,
            "fix pass for PROJ-1 failed: no comments captured"
        );
        assert!(cmds.is_empty());
    }

    // --- Msg::Tick on Screen::Board ---

    #[test]
    fn tick_on_board_is_a_noop_before_the_8th_tick() {
        let mut app = board_with(vec![], 0);
        let mut cmds = Vec::new();
        for _ in 0..7 {
            let (next_app, next_cmds) = update(app, Msg::Tick);
            app = next_app;
            cmds = next_cmds;
        }
        assert!(cmds.is_empty());
        assert_eq!(app.watch_tick, 7);
    }

    #[test]
    fn tick_on_board_emits_load_audit_status_every_8th_tick() {
        let mut app = board_with(vec![], 0);
        let mut cmds = Vec::new();
        for _ in 0..8 {
            let (next_app, next_cmds) = update(app, Msg::Tick);
            app = next_app;
            cmds = next_cmds;
        }
        assert_eq!(
            cmds,
            vec![
                Cmd::LoadAuditStatus,
                Cmd::LoadLaneRunStatus,
                Cmd::LoadBotWatchStatus,
                Cmd::LoadCleanupStatus,
            ]
        );
    }

    #[test]
    fn tick_on_board_loads_ticket_run_detail_every_2nd_tick_while_open() {
        let mut app = App {
            show_run_detail: true,
            ..board_with(vec![ticket("PROJ-1")], 0)
        };
        let (next_app, cmds) = update(app, Msg::Tick);
        app = next_app;
        assert!(cmds.is_empty());

        let (app, cmds) = update(app, Msg::Tick);
        assert_eq!(app.watch_tick, 2);
        assert_eq!(
            cmds,
            vec![Cmd::LoadTicketRunDetail {
                key: "PROJ-1".to_string()
            }]
        );
    }

    #[test]
    fn tick_on_board_does_not_load_run_detail_when_overlay_is_closed() {
        let app = board_with(vec![ticket("PROJ-1")], 0);
        let (app, _) = update(app, Msg::Tick);
        let (_app, cmds) = update(app, Msg::Tick);
        assert!(cmds.is_empty());
    }

    // --- lane_names / with_lane_names ---

    #[test]
    fn with_lane_names_sets_lane_names() {
        let app = App::new().with_lane_names(vec!["backend".to_string(), "frontend".to_string()]);
        assert_eq!(app.lane_names, vec!["backend", "frontend"]);
    }

    #[test]
    fn app_defaults_to_no_lane_names() {
        assert!(App::new().lane_names.is_empty());
    }

    #[test]
    fn with_hidden_lane_count_sets_hidden_lane_count() {
        let app = App::new().with_hidden_lane_count(3);
        assert_eq!(app.hidden_lane_count, 3);
    }

    #[test]
    fn app_defaults_to_zero_hidden_lane_count() {
        assert_eq!(App::new().hidden_lane_count, 0);
    }

    // --- Msg::LaneRunAction / Msg::LanePicker* / Msg::LaneRunLaunchResult / Msg::LaneRunStatusLoaded ---

    #[test]
    fn lane_run_action_with_no_selected_ticket_is_a_noop() {
        let app = board_with(vec![], 0).with_lane_names(vec!["backend".to_string()]);
        let (_app, cmds) = update(app, Msg::LaneRunAction);
        assert!(cmds.is_empty());
    }

    #[test]
    fn lane_run_action_with_zero_lanes_sets_status_message() {
        let app = board_with(vec![ticket("PROJ-1")], 0);
        let (app, cmds) = update(app, Msg::LaneRunAction);
        assert_eq!(app.status_line, "no lanes configured");
        assert!(cmds.is_empty());
    }

    #[test]
    fn lane_run_action_with_zero_lanes_and_hidden_lanes_notes_the_backend_mismatch() {
        // GitHub issue #5 phase 2: when every configured lane was filtered
        // out for a backend mismatch, the status line should say so rather
        // than claim nothing is configured at all.
        let app = board_with(vec![ticket("PROJ-1")], 0).with_hidden_lane_count(2);
        let (app, cmds) = update(app, Msg::LaneRunAction);
        assert_eq!(
            app.status_line,
            "no compatible lanes (2 hidden: backend mismatch)"
        );
        assert!(cmds.is_empty());
    }

    #[test]
    fn lane_run_action_with_one_lane_launches_directly_and_marks_pending() {
        let app =
            board_with(vec![ticket("PROJ-1")], 0).with_lane_names(vec!["backend".to_string()]);
        let (app, cmds) = update(app, Msg::LaneRunAction);
        assert_eq!(
            cmds,
            vec![Cmd::LaunchLaneRun {
                lane: "backend".to_string(),
                key: "PROJ-1".to_string(),
            }]
        );
        assert!(app.pending_lane_launches.contains("PROJ-1"));
        assert!(!app.show_lane_picker);
    }

    #[test]
    fn lane_run_action_with_multiple_lanes_opens_picker() {
        let app = board_with(vec![ticket("PROJ-1")], 0)
            .with_lane_names(vec!["backend".to_string(), "frontend".to_string()]);
        let (app, cmds) = update(app, Msg::LaneRunAction);
        assert!(app.show_lane_picker);
        assert_eq!(app.lane_picker_selected, 0);
        assert!(cmds.is_empty());
    }

    #[test]
    fn lane_run_action_with_pending_launch_sets_message_and_no_picker() {
        let mut app = board_with(vec![ticket("PROJ-1")], 0)
            .with_lane_names(vec!["backend".to_string(), "frontend".to_string()]);
        app.pending_lane_launches.insert("PROJ-1".to_string());
        let (app, cmds) = update(app, Msg::LaneRunAction);
        assert_eq!(app.status_line, "lane run already active for PROJ-1");
        assert!(!app.show_lane_picker);
        assert!(cmds.is_empty());
    }

    #[test]
    fn lane_run_action_with_active_indicator_sets_message_and_no_picker() {
        for indicator in [
            RunIndicator::Starting,
            RunIndicator::Running,
            RunIndicator::Waiting,
        ] {
            let mut app = board_with(vec![ticket("PROJ-1")], 0)
                .with_lane_names(vec!["backend".to_string(), "frontend".to_string()]);
            app.lane_run_status.insert("PROJ-1".to_string(), indicator);
            let (app, cmds) = update(app, Msg::LaneRunAction);
            assert_eq!(app.status_line, "lane run already active for PROJ-1");
            assert!(!app.show_lane_picker);
            assert!(cmds.is_empty());
        }
    }

    #[test]
    fn lane_run_action_with_terminal_indicator_relaunches() {
        for indicator in [RunIndicator::Done, RunIndicator::Failed] {
            let mut app =
                board_with(vec![ticket("PROJ-1")], 0).with_lane_names(vec!["backend".to_string()]);
            app.lane_run_status.insert("PROJ-1".to_string(), indicator);
            let (_app, cmds) = update(app, Msg::LaneRunAction);
            assert_eq!(
                cmds,
                vec![Cmd::LaunchLaneRun {
                    lane: "backend".to_string(),
                    key: "PROJ-1".to_string(),
                }]
            );
        }
    }

    #[test]
    fn lane_picker_up_and_down_navigate_and_clamp() {
        let app = App {
            show_lane_picker: true,
            lane_picker_selected: 0,
            lane_names: vec!["backend".to_string(), "frontend".to_string()],
            ..board_with(vec![ticket("PROJ-1")], 0)
        };
        let (app, _) = update(app, Msg::LanePickerUp);
        assert_eq!(app.lane_picker_selected, 0);
        let (app, _) = update(app, Msg::LanePickerDown);
        assert_eq!(app.lane_picker_selected, 1);
        let (app, _) = update(app, Msg::LanePickerDown);
        assert_eq!(app.lane_picker_selected, 1);
        let (app, _) = update(app, Msg::LanePickerUp);
        assert_eq!(app.lane_picker_selected, 0);
    }

    #[test]
    fn lane_picker_select_launches_chosen_lane_and_closes_picker() {
        let app = App {
            show_lane_picker: true,
            lane_picker_selected: 1,
            lane_names: vec!["backend".to_string(), "frontend".to_string()],
            ..board_with(vec![ticket("PROJ-1")], 0)
        };
        let (app, cmds) = update(app, Msg::LanePickerSelect);
        assert!(!app.show_lane_picker);
        assert_eq!(
            cmds,
            vec![Cmd::LaunchLaneRun {
                lane: "frontend".to_string(),
                key: "PROJ-1".to_string(),
            }]
        );
        assert!(app.pending_lane_launches.contains("PROJ-1"));
    }

    #[test]
    fn lane_picker_select_out_of_range_is_a_noop() {
        let app = App {
            show_lane_picker: true,
            lane_picker_selected: 10,
            lane_names: vec!["backend".to_string()],
            ..board_with(vec![ticket("PROJ-1")], 0)
        };
        let (app, cmds) = update(app, Msg::LanePickerSelect);
        assert!(app.show_lane_picker);
        assert!(cmds.is_empty());
    }

    #[test]
    fn lane_picker_close_leaves_state_unchanged() {
        let app = App {
            show_lane_picker: true,
            lane_names: vec!["backend".to_string()],
            ..board_with(vec![ticket("PROJ-1")], 0)
        };
        let (app, cmds) = update(app, Msg::LanePickerClose);
        assert!(!app.show_lane_picker);
        assert!(app.pending_lane_launches.is_empty());
        assert!(cmds.is_empty());
    }

    #[test]
    fn lane_run_launch_result_ok_removes_pending_and_sets_message() {
        let mut app = App::new();
        app.pending_lane_launches.insert("PROJ-1".to_string());
        let (app, cmds) = update(
            app,
            Msg::LaneRunLaunchResult {
                key: "PROJ-1".to_string(),
                result: Ok(()),
            },
        );
        assert!(!app.pending_lane_launches.contains("PROJ-1"));
        assert_eq!(app.status_line, "launched lane run for PROJ-1");
        assert!(cmds.is_empty());
    }

    #[test]
    fn lane_run_launch_result_err_removes_pending_and_sets_message() {
        let mut app = App::new();
        app.pending_lane_launches.insert("PROJ-1".to_string());
        let (app, cmds) = update(
            app,
            Msg::LaneRunLaunchResult {
                key: "PROJ-1".to_string(),
                result: Err("boom".to_string()),
            },
        );
        assert!(!app.pending_lane_launches.contains("PROJ-1"));
        assert_eq!(app.status_line, "lane run launch failed for PROJ-1: boom");
        assert!(cmds.is_empty());
    }

    #[test]
    fn lane_run_status_loaded_replaces_lane_run_status() {
        let app = App::new();
        let mut status = HashMap::new();
        status.insert("PROJ-1".to_string(), RunIndicator::Running);
        let (app, cmds) = update(app, Msg::LaneRunStatusLoaded(status.clone()));
        assert_eq!(app.lane_run_status, status);
        assert!(cmds.is_empty());
    }

    #[test]
    fn lane_run_status_loaded_overlays_starting_for_pending_with_no_run_row() {
        // The executor's `load_lane_run_status` has no access to
        // `pending_lane_launches` (it only sees `TuiDeps`), so a ticket
        // whose launcher child is still in flight but has no run row yet
        // would otherwise vanish from `lane_run_status` -- and therefore
        // from the board's badge, which reads only that map (see
        // `crate::tui::ui`) -- on every `Cmd::LoadLaneRunStatus` refresh.
        // `update` overlays `RunIndicator::Starting` here instead.
        let mut app = App::new();
        app.pending_lane_launches.insert("PROJ-1".to_string());
        let status = HashMap::new();
        let (app, cmds) = update(app, Msg::LaneRunStatusLoaded(status));
        assert_eq!(
            app.lane_run_status.get("PROJ-1"),
            Some(&RunIndicator::Starting)
        );
        assert!(cmds.is_empty());
    }

    #[test]
    fn lane_run_status_loaded_prefers_loaded_run_row_over_pending_overlay() {
        // Once the run row exists, its real status wins over the
        // pending-launch overlay -- the ticket stays pending only until its
        // launch result arrives, but the badge should reflect the run row
        // as soon as one shows up.
        let mut app = App::new();
        app.pending_lane_launches.insert("PROJ-1".to_string());
        let mut status = HashMap::new();
        status.insert("PROJ-1".to_string(), RunIndicator::Running);
        let (app, _cmds) = update(app, Msg::LaneRunStatusLoaded(status));
        assert_eq!(
            app.lane_run_status.get("PROJ-1"),
            Some(&RunIndicator::Running)
        );
    }

    fn retro_row(key: &str) -> RetroRow {
        RetroRow {
            key: key.to_string(),
            summary: format!("Summary for {key}"),
            url: format!("https://example.atlassian.net/browse/{key}"),
            run: None,
        }
    }

    fn retro_board_with(rows: Vec<RetroRow>, selected: usize) -> App {
        App {
            screen: Screen::Retro,
            retro_tickets: rows,
            retro_selected: selected,
            ..App::new()
        }
    }

    #[test]
    fn open_retro_switches_screen_and_fetches_shipped_jql() {
        let mut app = App::new();
        app.project_key = "PROJ".to_string();
        let (app, cmds) = update(app, Msg::OpenRetro);
        assert_eq!(app.screen, Screen::Retro);
        assert_eq!(
            cmds,
            vec![Cmd::FetchRetroTickets {
                query: TicketQuery::ShippedAwaitingRetro {
                    project_key: "PROJ".to_string()
                }
            }]
        );
    }

    #[test]
    fn open_retro_resets_defect_flow_state() {
        let mut app = App::new();
        app.show_retro_severity_picker = true;
        app.show_retro_note_entry = true;
        app.retro_note_draft = "leftover".to_string();
        app.retro_action_key = Some("PROJ-1".to_string());
        app.retro_pending_severity = Some(RetroSeverity::Major);
        let (app, _cmds) = update(app, Msg::OpenRetro);
        assert!(!app.show_retro_severity_picker);
        assert!(!app.show_retro_note_entry);
        assert!(app.retro_note_draft.is_empty());
        assert_eq!(app.retro_action_key, None);
        assert_eq!(app.retro_pending_severity, None);
    }

    #[test]
    fn retro_tickets_loaded_replaces_list_and_clamps_selection() {
        let app = retro_board_with(vec![retro_row("PROJ-1"), retro_row("PROJ-2")], 1);
        let (app, cmds) = update(app, Msg::RetroTicketsLoaded(vec![retro_row("PROJ-3")]));
        assert_eq!(app.retro_tickets, vec![retro_row("PROJ-3")]);
        assert_eq!(app.retro_selected, 0);
        assert!(cmds.is_empty());
    }

    #[test]
    fn retro_tickets_loaded_preserves_selection_by_key_when_still_present() {
        let app = retro_board_with(vec![retro_row("PROJ-1"), retro_row("PROJ-2")], 1);
        let (app, _) = update(
            app,
            Msg::RetroTicketsLoaded(vec![
                retro_row("PROJ-2"),
                retro_row("PROJ-1"),
                retro_row("PROJ-3"),
            ]),
        );
        assert_eq!(app.retro_selected, 0);
    }

    #[test]
    fn retro_tickets_failed_sets_status_line() {
        let app = retro_board_with(vec![], 0);
        let (app, cmds) = update(app, Msg::RetroTicketsFailed("boom".to_string()));
        assert_eq!(app.status_line, "boom");
        assert!(cmds.is_empty());
    }

    #[test]
    fn move_up_and_down_navigate_the_retro_list() {
        let app = retro_board_with(vec![retro_row("PROJ-1"), retro_row("PROJ-2")], 0);
        let (app, _) = update(app, Msg::Down);
        assert_eq!(app.retro_selected, 1);
        let (app, _) = update(app, Msg::Down);
        assert_eq!(app.retro_selected, 1, "should clamp at the last row");
        let (app, _) = update(app, Msg::Up);
        assert_eq!(app.retro_selected, 0);
        let (app, _) = update(app, Msg::Up);
        assert_eq!(app.retro_selected, 0, "should clamp at the first row");
    }

    #[test]
    fn back_on_retro_screen_returns_to_board() {
        let app = retro_board_with(vec![], 0);
        let (app, cmds) = update(app, Msg::Back);
        assert_eq!(app.screen, Screen::Board);
        assert!(cmds.is_empty());
    }

    #[test]
    fn enter_on_retro_screen_is_a_noop() {
        let app = retro_board_with(vec![retro_row("PROJ-1")], 0);
        let (app, cmds) = update(app, Msg::Enter);
        assert_eq!(app.screen, Screen::Retro);
        assert!(cmds.is_empty());
    }

    #[test]
    fn retro_defect_start_captures_key_and_opens_severity_picker() {
        let app = retro_board_with(vec![retro_row("PROJ-1"), retro_row("PROJ-2")], 1);
        let (app, cmds) = update(app, Msg::RetroDefectStart);
        assert_eq!(app.retro_action_key, Some("PROJ-2".to_string()));
        assert!(app.show_retro_severity_picker);
        assert_eq!(app.retro_severity_selected, 0);
        assert!(cmds.is_empty());
    }

    #[test]
    fn retro_defect_start_with_no_selection_is_a_noop() {
        let app = retro_board_with(vec![], 0);
        let (app, cmds) = update(app, Msg::RetroDefectStart);
        assert!(!app.show_retro_severity_picker);
        assert_eq!(app.retro_action_key, None);
        assert!(cmds.is_empty());
    }

    #[test]
    fn retro_severity_picker_navigation_clamps_at_bounds() {
        let mut app = retro_board_with(vec![retro_row("PROJ-1")], 0);
        app.show_retro_severity_picker = true;
        let (app, _) = update(app, Msg::RetroSeverityPickerUp);
        assert_eq!(app.retro_severity_selected, 0, "should clamp at zero");
        let (app, _) = update(app, Msg::RetroSeverityPickerDown);
        let (app, _) = update(app, Msg::RetroSeverityPickerDown);
        let (app, _) = update(app, Msg::RetroSeverityPickerDown);
        assert_eq!(
            app.retro_severity_selected,
            RETRO_SEVERITIES.len() - 1,
            "should clamp at the last severity"
        );
    }

    #[test]
    fn retro_severity_picker_select_moves_to_note_entry() {
        let mut app = retro_board_with(vec![retro_row("PROJ-1")], 0);
        app.retro_action_key = Some("PROJ-1".to_string());
        app.show_retro_severity_picker = true;
        app.retro_severity_selected = 1;
        let (app, cmds) = update(app, Msg::RetroSeverityPickerSelect);
        assert!(!app.show_retro_severity_picker);
        assert!(app.show_retro_note_entry);
        assert_eq!(app.retro_pending_severity, Some(RetroSeverity::Major));
        assert!(app.retro_note_draft.is_empty());
        assert!(cmds.is_empty());
    }

    #[test]
    fn retro_severity_picker_select_with_no_action_key_is_a_noop() {
        let mut app = retro_board_with(vec![], 0);
        app.show_retro_severity_picker = true;
        let (app, _) = update(app, Msg::RetroSeverityPickerSelect);
        assert!(app.show_retro_severity_picker, "picker should stay open");
        assert!(!app.show_retro_note_entry);
    }

    #[test]
    fn retro_severity_picker_close_discards_the_flow() {
        let mut app = retro_board_with(vec![], 0);
        app.show_retro_severity_picker = true;
        app.retro_action_key = Some("PROJ-1".to_string());
        let (app, cmds) = update(app, Msg::RetroSeverityPickerClose);
        assert!(!app.show_retro_severity_picker);
        assert_eq!(app.retro_action_key, None);
        assert!(cmds.is_empty());
    }

    #[test]
    fn retro_note_char_and_backspace_edit_the_draft() {
        let app = retro_board_with(vec![], 0);
        let (app, _) = update(app, Msg::RetroNoteChar('h'));
        let (app, _) = update(app, Msg::RetroNoteChar('i'));
        assert_eq!(app.retro_note_draft, "hi");
        let (app, _) = update(app, Msg::RetroNoteBackspace);
        assert_eq!(app.retro_note_draft, "h");
    }

    #[test]
    fn retro_note_submit_with_text_records_defect_with_note() {
        let mut app = retro_board_with(vec![], 0);
        app.retro_action_key = Some("PROJ-1".to_string());
        app.retro_pending_severity = Some(RetroSeverity::Critical);
        app.show_retro_note_entry = true;
        app.retro_note_draft = "  it broke prod  ".to_string();
        let (app, cmds) = update(app, Msg::RetroNoteSubmit);
        assert!(!app.show_retro_note_entry);
        assert_eq!(app.retro_action_key, None);
        assert_eq!(app.retro_pending_severity, None);
        assert_eq!(
            cmds,
            vec![Cmd::RecordRetro {
                key: "PROJ-1".to_string(),
                verdict: RetroVerdict::Defect,
                severity: Some(RetroSeverity::Critical),
                notes: Some("it broke prod".to_string()),
            }]
        );
    }

    #[test]
    fn retro_note_submit_with_blank_text_records_no_note() {
        let mut app = retro_board_with(vec![], 0);
        app.retro_action_key = Some("PROJ-1".to_string());
        app.retro_pending_severity = Some(RetroSeverity::Minor);
        app.show_retro_note_entry = true;
        app.retro_note_draft = "   ".to_string();
        let (_app, cmds) = update(app, Msg::RetroNoteSubmit);
        assert_eq!(
            cmds,
            vec![Cmd::RecordRetro {
                key: "PROJ-1".to_string(),
                verdict: RetroVerdict::Defect,
                severity: Some(RetroSeverity::Minor),
                notes: None,
            }]
        );
    }

    #[test]
    fn retro_note_submit_with_missing_flow_state_is_a_noop() {
        let app = retro_board_with(vec![], 0);
        let (app, cmds) = update(app, Msg::RetroNoteSubmit);
        assert!(cmds.is_empty());
        assert!(!app.show_retro_note_entry);
    }

    #[test]
    fn retro_note_cancel_discards_the_flow() {
        let mut app = retro_board_with(vec![], 0);
        app.show_retro_note_entry = true;
        app.retro_action_key = Some("PROJ-1".to_string());
        app.retro_pending_severity = Some(RetroSeverity::Major);
        app.retro_note_draft = "typed something".to_string();
        let (app, cmds) = update(app, Msg::RetroNoteCancel);
        assert!(!app.show_retro_note_entry);
        assert_eq!(app.retro_action_key, None);
        assert_eq!(app.retro_pending_severity, None);
        assert!(app.retro_note_draft.is_empty());
        assert!(cmds.is_empty());
    }

    #[test]
    fn retro_mark_clean_emits_record_retro_with_no_severity() {
        let app = retro_board_with(vec![retro_row("PROJ-1")], 0);
        let (_app, cmds) = update(app, Msg::RetroMarkClean);
        assert_eq!(
            cmds,
            vec![Cmd::RecordRetro {
                key: "PROJ-1".to_string(),
                verdict: RetroVerdict::Clean,
                severity: None,
                notes: None,
            }]
        );
    }

    #[test]
    fn retro_mark_clean_with_no_selection_is_a_noop() {
        let app = retro_board_with(vec![], 0);
        let (_app, cmds) = update(app, Msg::RetroMarkClean);
        assert!(cmds.is_empty());
    }

    #[test]
    fn retro_recorded_drops_the_ticket_and_sets_status_line() {
        let app = retro_board_with(vec![retro_row("PROJ-1"), retro_row("PROJ-2")], 1);
        let (app, cmds) = update(
            app,
            Msg::RetroRecorded {
                key: "PROJ-2".to_string(),
                verdict: RetroVerdict::Clean,
            },
        );
        assert_eq!(app.retro_tickets, vec![retro_row("PROJ-1")]);
        assert_eq!(app.retro_selected, 0);
        assert_eq!(app.status_line, "Recorded clean for PROJ-2");
        assert!(cmds.is_empty());
    }

    #[test]
    fn retro_failed_sets_status_line_and_keeps_the_ticket() {
        let app = retro_board_with(vec![retro_row("PROJ-1")], 0);
        let (app, cmds) = update(app, Msg::RetroFailed("db locked".to_string()));
        assert_eq!(app.status_line, "db locked");
        assert_eq!(app.retro_tickets, vec![retro_row("PROJ-1")]);
        assert!(cmds.is_empty());
    }
}
