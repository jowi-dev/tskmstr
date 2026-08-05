//! Pure Elm-style state and reducer for the TUI.
//!
//! [`App`] holds all UI state, [`Msg`] is every event the reducer can react
//! to, and [`update`] is the single place state transitions happen. No I/O
//! occurs here: [`Cmd`] values name the I/O the caller (`crate::tui::event`)
//! should perform next, and *Failed messages let callers feed I/O errors back
//! in without `update` ever needing to panic.

use crate::jira::client::RankAnchor;
use crate::jira::jql::{
    assignee_tickets_jql, everyone_tickets_jql, my_open_tickets_jql, ranked_tickets_jql,
    unassigned_tickets_jql,
};
use crate::jira::types::{JiraUser, Transition};

/// A ticket as displayed on the board, derived from a
/// [`crate::jira::types::Issue`] plus the configured Jira base URL.
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
    /// Plain-text description, extracted from the issue's ADF description
    /// via [`crate::jira::adf::adf_to_text`].
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

/// Build the JQL query for `filter`, scoping project-wide filters to
/// `project_key`. [`AssigneeFilter::Me`] ignores `project_key` entirely,
/// preserving the board's original, unscoped query.
pub fn jql_for_filter(filter: &AssigneeFilter, project_key: &str) -> String {
    match filter {
        AssigneeFilter::Me => my_open_tickets_jql(),
        AssigneeFilter::Unassigned => unassigned_tickets_jql(project_key),
        AssigneeFilter::Everyone => everyone_tickets_jql(project_key),
        AssigneeFilter::User(user) => assignee_tickets_jql(project_key, &user.account_id),
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
/// Columns are ordered by status category rank (new, then indeterminate,
/// then done, then unknown categories), and alphabetically by status name
/// within a category. Tickets keep their relative fetch order within a
/// column.
pub fn group_into_columns(tickets: Vec<TicketSummary>) -> Vec<Column> {
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

    columns.sort_by(|a, b| {
        let rank_a = category_rank(&a.tickets[0].status_category);
        let rank_b = category_rank(&b.tickets[0].status_category);
        rank_a.cmp(&rank_b).then_with(|| a.title.cmp(&b.title))
    });

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

/// The fixed column order for [`Screen::Runs`]'s kanban board. All six
/// columns always render, even when empty.
pub const RUN_COLUMNS: [crate::runs::RunStatus; 6] = [
    crate::runs::RunStatus::Queued,
    crate::runs::RunStatus::Running,
    crate::runs::RunStatus::Blocked,
    crate::runs::RunStatus::Review,
    crate::runs::RunStatus::Done,
    crate::runs::RunStatus::Failed,
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
    /// Number of [`Msg::Tick`]s processed since `tm runs watch` started,
    /// used to throttle polling and periodic reaping.
    pub watch_tick: u64,
}

impl App {
    /// An app with no tickets, showing the board.
    pub fn new() -> Self {
        Self::default()
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
    /// Open the selected ticket's URL in a browser.
    OpenInBrowser,
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
    /// The ticket list finished loading.
    TicketsLoaded(Vec<TicketSummary>),
    /// The ticket list failed to load.
    TicketsFailed(String),
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
    /// The run detail window's data failed to load.
    RunDetailFailed(String),
    /// A reap pass completed, having reaped `0` reports as a no-op status
    /// line.
    RunsReaped(usize),
}

/// I/O the caller should perform as a result of [`update`].
///
/// `update` itself never performs I/O; it only describes what should happen.
/// The caller (`crate::tui::event`) executes each `Cmd` and feeds the
/// resulting `Msg` back through `update`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    /// Fetch tickets matching `jql`, built by [`jql_for_filter`] from the
    /// board's active [`AssigneeFilter`].
    FetchTickets {
        /// The JQL query to search with.
        jql: String,
    },
    /// Fetch assignable users for `project`, for the filter picker.
    FetchAssignableUsers {
        /// Project key to list assignable users for.
        project: String,
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
        /// The JQL query to search with (built by [`ranked_tickets_jql`]).
        jql: String,
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
    /// Reap abandoned runs in the run store.
    ReapRuns,
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
                let jql = ranked_tickets_jql(&app.project_key);
                (app, vec![Cmd::FetchRankTickets { jql }])
            } else {
                let jql = jql_for_filter(&app.filter, &app.project_key);
                (app, vec![Cmd::FetchTickets { jql }])
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
            app.columns = group_into_columns(tickets);
            reselect(&mut app, preferred_key);
            (app, Vec::new())
        }
        Msg::TicketsFailed(err) => {
            app.status_line = err;
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
            app.columns = group_into_columns(tickets);
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
            let count = app.filter_options().len();
            if count > 0 {
                app.filter_picker_selected = app.filter_picker_selected.min(count - 1);
            }
            (app, Vec::new())
        }
        Msg::AssignableUsersFailed(err) => {
            app.filter_picker_error = Some(err);
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
            let jql = ranked_tickets_jql(&app.project_key);
            (app, vec![Cmd::FetchRankTickets { jql }])
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
            (app, Vec::new())
        }
        Msg::RunsReaped(count) => {
            if count > 0 {
                app.status_line = format!("Reaped {count} dead run(s)");
            }
            (app, Vec::new())
        }
    }
}

/// Handle [`Msg::Tick`]: a no-op off [`Screen::Runs`]. On it, increments
/// `watch_tick` and emits [`Cmd::LoadRuns`] every 2nd tick (plus
/// [`Cmd::LoadRunDetail`] when the detail window is open) and
/// [`Cmd::ReapRuns`] every 120th tick (~30s at the 250ms poll interval).
fn tick(mut app: App) -> (App, Vec<Cmd>) {
    if app.screen != Screen::Runs {
        return (app, Vec::new());
    }

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
    let jql = ranked_tickets_jql(&app.project_key);
    (app, vec![Cmd::FetchRankTickets { jql }])
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
    let jql = jql_for_filter(&app.filter, &app.project_key);
    (app, vec![Cmd::FetchTickets { jql }])
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
    }
}

/// Handle [`Msg::Back`]: step back a screen, or quit from the board. On the
/// rank screen, cancels an in-progress grab instead of leaving the screen if
/// one is active (so `Esc`/`q` can never quit or navigate away mid-grab). On
/// the runs screen, closes the detail window if one is open instead of
/// quitting (`tm runs watch` has no screen to fall back to, so `Back` quits
/// only once the window is already closed).
fn back(app: &mut App) {
    match app.screen {
        Screen::Board => app.quit = true,
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
    }
}

/// Move the current selection/scroll up by one, saturating at the top. On
/// the rank screen, moves the grabbed ticket itself (cursor follows it)
/// instead of just the cursor while a grab is active.
fn move_up(app: &mut App) {
    match app.screen {
        Screen::Board => app.selected_row = app.selected_row.saturating_sub(1),
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
    }
}

/// Move the current selection/scroll down by one, clamping at the bottom of
/// the relevant list. Detail scroll has no known upper bound at this layer,
/// so it is left to increase; the detail view clamps what it displays.
fn move_down(app: &mut App) {
    match app.screen {
        Screen::Board => {
            if let Some(len) = current_column_len(app) {
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
            columns: group_into_columns(tickets),
            selected_col: 0,
            selected_row,
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
            columns: group_into_columns(tickets),
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
                jql: my_open_tickets_jql()
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

    #[test]
    fn toggle_help_flips_show_help() {
        let app = App::new();
        let (app, _) = update(app, Msg::ToggleHelp);
        assert!(app.show_help);
        let (app, _) = update(app, Msg::ToggleHelp);
        assert!(!app.show_help);
    }

    fn transition(id: &str, name: &str) -> Transition {
        use crate::jira::types::{Status, StatusCategory};

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
            columns: group_into_columns(tickets),
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
            let columns = group_into_columns(case.tickets);
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
                jql: unassigned_tickets_jql("PROJ")
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
                jql: everyone_tickets_jql("PROJ")
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
                jql: assignee_tickets_jql("PROJ", "acct-1")
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
                jql: my_open_tickets_jql()
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
                jql: ranked_tickets_jql("PROJ")
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
                jql: ranked_tickets_jql("PROJ")
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
                jql: my_open_tickets_jql()
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
                jql: ranked_tickets_jql("PROJ")
            }]
        );
    }

    fn run_card(id: i64, ticket: &str, status: crate::runs::RunStatus) -> RunCard {
        RunCard {
            id,
            ticket: ticket.to_string(),
            lane: "backend".to_string(),
            status,
            age_secs: 10,
            heartbeat_age_secs: Some(5),
            last_event_kind: None,
            last_event_age_secs: None,
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
        let app = App::new();
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
}
