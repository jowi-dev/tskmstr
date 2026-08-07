//! Terminal wiring: the only module in `tui` that touches a real terminal or
//! performs network/process I/O.
//!
//! [`run`] owns the event loop; [`execute`] is the thin translation from a
//! [`Cmd`] to the [`Msg`] it produces, kept separate so it can be unit tested
//! with [`crate::jira::fake::FakeJiraClient`] instead of a live Jira and a
//! real terminal.
//!
//! [`Cmd::AttachAudit`] is the one exception to that split: attaching needs
//! `&mut Terminal` to suspend and restore the alternate screen around the
//! blocking `tmux attach-session` call, which `execute`'s signature has no
//! access to (and shouldn't grow one just for this). [`run_cmds`] intercepts
//! it before it ever reaches `execute`, handles the suspend/restore itself
//! (see [`attach_audit`]), and feeds the result back through `update` as an
//! ordinary [`Msg`] -- every other `Cmd` still flows through `execute`
//! unchanged.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use crossterm::event::{self, Event as CEvent, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};
use thiserror::Error;

use crate::jira::adf::adf_to_text;
use crate::jira::client::{JiraClient, JiraError, RankAnchor};
use crate::jira::types::Issue;
use crate::tui::app::{App, AuditStatusEntry, Cmd, Msg, TicketSummary, audit_indicator};
use crate::tui::app::{jql_for_filter, update};
use crate::tui::keymap::map_key;
use crate::tui::ui::draw;
use crate::work::tmux::TmuxOps;

/// How long to wait for a key press between redraws.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Errors that can occur while running the TUI.
#[derive(Debug, Error)]
pub enum TuiError {
    /// Setting up or tearing down the terminal, or drawing to it, failed.
    #[error("terminal I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Dependencies the TUI needs to talk to Jira, build browsable URLs, and
/// (per `docs/plans/board-audits.md`'s "Board integration" design) launch
/// and attach to board-launched ticket-audit sessions.
pub struct TuiDeps {
    /// Client used to fetch tickets, transitions, and apply transitions.
    pub jira: Box<dyn JiraClient>,
    /// Base URL of the Jira instance, used to build `{base_url}/browse/{key}`
    /// links for [`Cmd::OpenUrl`].
    pub base_url: String,
    /// The configured default Jira project key, used to scope every
    /// assignee filter other than `Me`.
    pub project_key: String,
    /// Configured board column order, from
    /// [`crate::config::Config::board_column_order`]. Empty when
    /// unconfigured, leaving the board's default column ordering unchanged.
    pub board_column_order: Vec<String>,
    /// Handle to the run-state database, or `None` if it failed to open.
    /// Lenient by design (mirroring
    /// [`crate::cli::ticket::AuditStoreStatus`]'s stance for `tm ticket
    /// audit`'s read mode): a broken runs DB must never block the Jira
    /// board itself, only degrade audit status/launch to unavailable.
    pub store: Option<crate::runs::RunStore>,
    /// tmux operations used to list/launch/attach audit sessions.
    pub tmux: Box<dyn TmuxOps>,
    /// Validated `[work.audit]` settings; launching is disabled (a
    /// status-line error) when `dir` is unset. See
    /// [`crate::work::audit::launch_audit`].
    pub audit: crate::config::AuditConfig,
    /// The user's home directory, used to expand a leading `~` in
    /// `audit.dir`.
    pub home: std::path::PathBuf,
}

/// Restores the terminal (raw mode and the alternate screen) when dropped.
///
/// Constructed immediately after `enable_raw_mode` succeeds, so the terminal
/// is restored whether `run` returns normally, returns an error, or the
/// current thread panics while the guard is in scope (`Drop` still runs
/// during unwinding).
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
    }
}

/// Run the interactive board until the user quits.
///
/// Enters raw mode and the alternate screen, fetches the initial ticket list
/// and audit status, then loops: draw the current screen, wait up to
/// `POLL_INTERVAL` for a key press. A key press maps to a [`Msg`] which runs
/// through [`crate::tui::app::update`], executing any resulting [`Cmd`]s; a
/// timed-out poll feeds [`Msg::Tick`] through the same path (mirroring
/// [`run_watch`]), which is what drives [`Screen::Board`]'s periodic
/// [`Cmd::LoadAuditStatus`] polling. The terminal is always restored before
/// returning, including on error.
///
/// [`Screen::Board`]: crate::tui::app::Screen::Board
pub fn run(deps: TuiDeps) -> Result<(), TuiError> {
    enable_raw_mode()?;
    let _guard = TerminalGuard;
    execute!(std::io::stdout(), EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut app = App {
        project_key: deps.project_key.clone(),
        board_column_order: deps.board_column_order.clone(),
        ..App::new()
    };
    let jql = jql_for_filter(&app.filter, &app.project_key);
    app = run_cmds(
        app,
        vec![Cmd::FetchTickets { jql }, Cmd::LoadAuditStatus],
        &deps,
        &mut terminal,
    );

    while !app.quit {
        terminal.draw(|frame| draw(frame, &app))?;

        if event::poll(POLL_INTERVAL)? {
            if let CEvent::Key(key_event) = event::read()?
                && key_event.kind == KeyEventKind::Press
                && let Some(msg) = map_key(
                    &app.screen,
                    app.show_help,
                    app.show_filter_picker,
                    app.is_rank_grabbed(),
                    app.show_run_detail,
                    key_event.code,
                )
            {
                let (next_app, cmds) = update(app, msg);
                app = run_cmds(next_app, cmds, &deps, &mut terminal);
            }
        } else {
            let (next_app, cmds) = update(app, Msg::Tick);
            app = run_cmds(next_app, cmds, &deps, &mut terminal);
        }
    }

    Ok(())
}

/// Execute every `Cmd` in `cmds`, feeding each resulting `Msg` back through
/// `update` (which may itself produce further `Cmd`s, e.g. loading
/// transitions after opening the detail screen).
///
/// [`Cmd::AttachAudit`] is intercepted here rather than passed to `execute`:
/// see the module docs. Generic over the terminal backend (rather than fixed
/// to [`CrosstermBackend`]) purely so tests can drive it with
/// `ratatui::backend::TestBackend` instead of a real terminal.
fn run_cmds<B: Backend>(
    mut app: App,
    cmds: Vec<Cmd>,
    deps: &TuiDeps,
    terminal: &mut Terminal<B>,
) -> App {
    let mut pending: VecDeque<Cmd> = cmds.into();
    while let Some(cmd) = pending.pop_front() {
        if let Cmd::AttachAudit { session_name } = cmd {
            let message = attach_audit(terminal, deps.tmux.as_ref(), &session_name);
            let (next_app, more_cmds) = update(app, Msg::AuditActionResult(message));
            app = next_app;
            pending.extend(more_cmds);
            continue;
        }
        for msg in execute(deps, cmd) {
            let (next_app, more_cmds) = update(app, msg);
            app = next_app;
            pending.extend(more_cmds);
        }
    }
    app
}

/// Suspend the board's alternate screen and raw mode, run `tmux
/// attach-session -t <session_name>` with inherited stdio (blocking until
/// the user detaches, e.g. `C-b d`, or the session ends), then restore the
/// alternate screen and raw mode and clear the terminal so the board redraws
/// cleanly. Returns a status-line message describing the outcome.
///
/// Ordering rationale: restore runs [`run`]'s setup operations in their
/// original order (`enable_raw_mode` then `EnterAlternateScreen`), and
/// suspend is its exact reverse (`LeaveAlternateScreen` then
/// `disable_raw_mode`), so the round trip unwinds and rebuilds the terminal
/// state symmetrically. Note this suspend order is the *reverse* of
/// [`TerminalGuard::drop`]'s (which disables raw mode first); both orders
/// restore a usable terminal, but the symmetric one is used here because
/// this pair must compose with a re-entry rather than end the program. Every step is best-effort (`let _ =`): if `tmux attach` itself
/// fails, the terminal must still be restored before the error is reported,
/// so a restore step failing too must not short-circuit the ones after it.
///
/// ## Manual test plan
///
/// The suspend/restore mechanics need a real terminal and a real `tmux`, so
/// (like `src/work/detach.rs`'s `RealDetachSpawner`, see its "What's
/// unit-tested vs. deferred to manual verification" section) they aren't
/// unit-tested; [`Cmd`] routing and the resulting status-line message are
/// (see this module's tests). Verify manually:
///
/// 1. Configure `[work.audit].dir` and launch a session from the board
///    (`a` on a ticket with no live audit session; the badge should read
///    `Starting` then `Running`).
/// 2. Press `a` again: confirm the board's alternate screen is left, the
///    terminal shows the `claude` session's own output, and typing works
///    normally (tmux's own raw mode takes over; this process's is off).
/// 3. Detach with `C-b d`: confirm the board's alternate screen re-enters,
///    the screen clears and redraws cleanly (no leftover tmux output visible
///    behind it), and the status line reads `detached from tm-audit-<key>`.
/// 4. Kill the tmux session from another terminal while attached (`tmux
///    kill-session -t tm-audit-<key>`); confirm `tmux attach-session`
///    exiting with an error still leaves this terminal fully usable (raw
///    mode re-enabled, alternate screen re-entered, board redrawn) rather
///    than stranding the shell.
fn attach_audit<B: Backend>(
    terminal: &mut Terminal<B>,
    tmux: &dyn TmuxOps,
    session_name: &str,
) -> String {
    let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
    let _ = disable_raw_mode();

    let result = tmux.attach(session_name);

    let _ = enable_raw_mode();
    let _ = execute!(std::io::stdout(), EnterAlternateScreen);
    let _ = terminal.clear();

    match result {
        Ok(()) => format!("detached from {session_name}"),
        Err(err) => format!("attach to {session_name} failed: {err}"),
    }
}

/// Dependencies for `tm runs watch`: just the run store.
///
/// Deliberately separate from [`TuiDeps`] rather than a superset of it: `tm
/// runs watch` is a local-only command and must not drag a Jira client or
/// token into scope just to satisfy a shared dependency struct.
pub struct WatchDeps {
    /// Store used to load and reap runs.
    pub store: crate::runs::RunStore,
}

/// Run the live runs kanban board until the user quits.
///
/// Mirrors [`run`]'s skeleton (raw mode, the alternate screen, a
/// [`TerminalGuard`], a [`POLL_INTERVAL`] poll loop), but starts on
/// [`crate::tui::app::Screen::Runs`], reaps and loads runs on startup, and
/// feeds [`Msg::Tick`] through [`update`] whenever a poll times out with no
/// key pressed (the board loop just redraws instead; the watch loop uses the
/// timeout as its clock for polling and periodic reaping).
pub fn run_watch(deps: WatchDeps) -> Result<(), TuiError> {
    enable_raw_mode()?;
    let _guard = TerminalGuard;
    execute!(std::io::stdout(), EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut app = App {
        screen: crate::tui::app::Screen::Runs,
        ..App::new()
    };
    app = run_watch_cmds(app, vec![Cmd::ReapRuns, Cmd::LoadRuns], &deps);

    while !app.quit {
        terminal.draw(|frame| draw(frame, &app))?;

        if event::poll(POLL_INTERVAL)? {
            if let CEvent::Key(key_event) = event::read()?
                && key_event.kind == KeyEventKind::Press
                && let Some(msg) = map_key(
                    &app.screen,
                    app.show_help,
                    app.show_filter_picker,
                    app.is_rank_grabbed(),
                    app.show_run_detail,
                    key_event.code,
                )
            {
                let (next_app, cmds) = update(app, msg);
                app = run_watch_cmds(next_app, cmds, &deps);
            }
        } else {
            let (next_app, cmds) = update(app, Msg::Tick);
            app = run_watch_cmds(next_app, cmds, &deps);
        }
    }

    Ok(())
}

/// Execute every `Cmd` in `cmds` against [`WatchDeps`], feeding each
/// resulting `Msg` back through `update` (which may itself produce further
/// `Cmd`s). Mirrors [`run_cmds`].
fn run_watch_cmds(mut app: App, cmds: Vec<Cmd>, deps: &WatchDeps) -> App {
    let mut pending: VecDeque<Cmd> = cmds.into();
    while let Some(cmd) = pending.pop_front() {
        for msg in execute_watch(deps, cmd) {
            let (next_app, more_cmds) = update(app, msg);
            app = next_app;
            pending.extend(more_cmds);
        }
    }
    app
}

/// Translate a single [`Cmd`] into the [`Msg`]s it produces, for `tm runs
/// watch`. Handles only the run-store `Cmd`s; every other variant is
/// unreachable from [`crate::tui::app::Screen::Runs`].
fn execute_watch(deps: &WatchDeps, cmd: Cmd) -> Vec<Msg> {
    match cmd {
        Cmd::LoadRuns => load_runs(deps),
        Cmd::LoadRunDetail { run_id } => load_run_detail(deps, run_id),
        Cmd::ReapRuns => reap_runs(deps),
        other => {
            debug_assert!(
                false,
                "execute_watch: unreachable Cmd from Screen::Runs: {other:?}"
            );
            Vec::new()
        }
    }
}

/// Run `Cmd::LoadRuns`: list every run and map it to a
/// [`crate::tui::app::RunCard`].
///
/// Fetches each run's full event timeline (via
/// [`crate::runs::RunStore::events_for_run`]) to compute its latest
/// checklist for the card's progress marker. This re-fetches events already
/// read for the (much rarer) detail window rather than adding a dedicated
/// `latest_event_of_kind` store query: run and per-run event counts are
/// small in this local SQLite store, `run_events` is already indexed by
/// `run_id`, and `LoadRuns` only fires every other tick (~500ms), so the
/// N+1 query pattern here stays cheap.
fn load_runs(deps: &WatchDeps) -> Vec<Msg> {
    match deps.store.list_runs() {
        Ok(summaries) => vec![Msg::RunsLoaded(
            summaries
                .into_iter()
                .map(|summary| run_summary_to_card(deps, summary))
                .collect(),
        )],
        Err(err) => vec![Msg::RunsFailed(err.to_string())],
    }
}

/// Map a [`crate::runs::RunSummary`] to a [`crate::tui::app::RunCard`],
/// attaching its latest checklist (if any) per [`load_runs`]'s doc comment.
fn run_summary_to_card(
    deps: &WatchDeps,
    summary: crate::runs::RunSummary,
) -> crate::tui::app::RunCard {
    let checklist = deps
        .store
        .events_for_run(summary.id)
        .ok()
        .and_then(|events| crate::runs::latest_checklist(&events));
    crate::tui::app::RunCard {
        id: summary.id,
        ticket: summary.ticket,
        lane: summary.lane,
        kind: summary.kind,
        status: summary.status,
        age_secs: summary.age_secs,
        heartbeat_age_secs: summary.heartbeat_age_secs,
        last_event_kind: summary.last_event_kind,
        last_event_age_secs: summary.last_event_age_secs,
        awaiting_input: summary.awaiting_input,
        checklist,
    }
}

/// Run `Cmd::LoadRunDetail`: load the run and its full event timeline for the
/// run detail floating window.
fn load_run_detail(deps: &WatchDeps, run_id: i64) -> Vec<Msg> {
    let run = match deps.store.run_by_id(run_id) {
        Ok(Some(run)) => run,
        Ok(None) => return vec![Msg::RunDetailFailed(format!("no run with id {run_id}"))],
        Err(err) => return vec![Msg::RunDetailFailed(err.to_string())],
    };
    let events = match deps.store.events_for_run(run_id) {
        Ok(events) => events,
        Err(err) => return vec![Msg::RunDetailFailed(err.to_string())],
    };
    vec![Msg::RunDetailLoaded(Box::new(run_to_detail(run, events)))]
}

/// Map a [`crate::runs::Run`] plus its events to a
/// [`crate::tui::app::RunDetail`].
fn run_to_detail(
    run: crate::runs::Run,
    events: Vec<crate::runs::RunEvent>,
) -> crate::tui::app::RunDetail {
    let checklist = crate::runs::latest_checklist(&events);
    let tool_counts = crate::runs::tool_counts(&events);
    let model_usage = run_model_usage(run.model_usage.as_deref(), run.status, &events);
    let agent_usage = crate::runs::format_agent_usage(&crate::runs::aggregate_agent_usage(
        &crate::runs::collect_agent_usage(&events),
    ));
    crate::tui::app::RunDetail {
        id: run.id,
        ticket: run.ticket,
        lane: run.lane,
        kind: run.kind,
        status: run.status,
        worktree: run.worktree,
        branch: run.branch,
        pid: run.pid,
        session_id: run.session_id,
        cost_usd: run.cost_usd,
        num_turns: run.num_turns,
        pr_url: run.pr_url,
        blocker: run.blocker,
        started_at: run.started_at,
        ended_at: run.ended_at,
        events: events
            .into_iter()
            .map(|e| crate::tui::app::RunDetailEvent {
                at: e.at,
                kind: e.kind,
                detail: e.detail,
            })
            .collect(),
        checklist,
        tool_counts,
        model_usage,
        agent_usage,
    }
}

/// Resolves a [`crate::tui::app::RunModelUsage`] for the run detail window:
/// prefers `model_usage_column` (the authoritative, cost-bearing snapshot
/// recorded by `tm runs finish --model-usage`), falling back to the
/// latest `usage` event's live snapshot while `status` is
/// [`crate::runs::RunStatus::Running`]. `None` when neither is available.
fn run_model_usage(
    model_usage_column: Option<&str>,
    status: crate::runs::RunStatus,
    events: &[crate::runs::RunEvent],
) -> Option<crate::tui::app::RunModelUsage> {
    if let Some(usage) = model_usage_column.and_then(crate::runs::parse_model_usage) {
        return Some(crate::tui::app::RunModelUsage {
            label: "Model usage",
            lines: crate::runs::format_model_usage(&usage),
        });
    }
    if status == crate::runs::RunStatus::Running
        && let Some(usage) = crate::runs::latest_usage(events)
    {
        return Some(crate::tui::app::RunModelUsage {
            label: "Model usage (live)",
            lines: crate::runs::format_model_usage(&usage),
        });
    }
    None
}

/// Run `Cmd::ReapRuns`: mark abandoned runs as failed, using the same
/// staleness threshold as `tm runs reap`'s default (10 minutes).
fn reap_runs(deps: &WatchDeps) -> Vec<Msg> {
    match deps.store.reap(10, &crate::runs::pid::pid_alive) {
        Ok(reaped) => vec![Msg::RunsReaped(reaped.len())],
        Err(err) => vec![Msg::RunsFailed(err.to_string())],
    }
}

/// Translate a single [`Cmd`] into the [`Msg`]s it produces.
///
/// Performs the actual I/O (a Jira API call, or spawning the `open`
/// process); everything else in `tui` stays pure and terminal-free.
fn execute(deps: &TuiDeps, cmd: Cmd) -> Vec<Msg> {
    match cmd {
        Cmd::FetchTickets { jql } => fetch_tickets(deps, &jql),
        Cmd::FetchAssignableUsers { project } => fetch_assignable_users(deps, &project),
        Cmd::FetchTransitions { key } => fetch_transitions(deps, &key),
        Cmd::ApplyTransition { key, transition_id } => apply_transition(deps, &key, &transition_id),
        Cmd::OpenUrl(url) => open_url(&url),
        Cmd::FetchRankTickets { jql } => fetch_rank_tickets(deps, &jql),
        Cmd::RankTicket { key, anchor } => rank_ticket(deps, &key, anchor),
        Cmd::LoadAuditStatus => load_audit_status(deps),
        Cmd::LaunchAudit { key } => launch_audit_cmd(deps, &key),
        // `Cmd::AttachAudit` needs `&mut Terminal` (to suspend/restore the
        // alternate screen around the blocking `tmux attach` call), which
        // this function's signature intentionally has no access to;
        // `run_cmds` always intercepts it before calling `execute` (see the
        // module docs), so it's unreachable here in practice.
        //
        // The Jira board never enters `Screen::Runs`, so `update` can never
        // produce one of the `Load*`/`Reap*` run-store `Cmd`s for
        // `run`/`execute` to handle either.
        other @ (Cmd::LoadRuns
        | Cmd::LoadRunDetail { .. }
        | Cmd::ReapRuns
        | Cmd::AttachAudit { .. }) => {
            debug_assert!(
                false,
                "execute: unreachable Cmd on the Jira board: {other:?}"
            );
            Vec::new()
        }
    }
}

/// Run `Cmd::LoadAuditStatus`: build the board's per-ticket audit badge map
/// from live `tm-audit-<key>` tmux sessions and the latest `kind = "audit"`
/// run per ticket, via [`audit_indicator`]'s pure precedence rule.
///
/// `deps.store` being `None` (an unopenable runs DB) yields an empty map
/// rather than an error -- the Jira board must keep working without run-store
/// access. A `tmux.list_sessions()` or `store.list_runs_filtered()` failure
/// is likewise treated as "nothing to report" rather than a board-wide error,
/// matching this command's role as best-effort background enrichment, not a
/// primary data load.
fn load_audit_status(deps: &TuiDeps) -> Vec<Msg> {
    let Some(store) = &deps.store else {
        return vec![Msg::AuditStatusLoaded(HashMap::new())];
    };

    let sessions: HashSet<String> = deps
        .tmux
        .list_sessions()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|session| audit_session_ticket_key(&session.name))
        .collect();

    // `list_runs_filtered` orders active runs before terminal ones, and by
    // `started_at` descending within each group (see
    // `crate::runs::RunStore::list_runs_filtered`), so the first run seen
    // per ticket here is exactly "the live run if one exists, otherwise the
    // most recent terminal one" -- precisely the run `audit_indicator` wants.
    let mut latest_by_ticket: HashMap<String, (crate::runs::RunStatus, bool)> = HashMap::new();
    for run in store.list_runs_filtered(Some("audit")).unwrap_or_default() {
        latest_by_ticket
            .entry(run.ticket)
            .or_insert((run.status, run.awaiting_input));
    }

    let mut keys: HashSet<&String> = sessions.iter().collect();
    keys.extend(latest_by_ticket.keys());

    let mut status = HashMap::new();
    for key in keys {
        let has_session = sessions.contains(key);
        let run = latest_by_ticket.get(key).copied();
        if let Some(indicator) = audit_indicator(has_session, run) {
            status.insert(
                key.clone(),
                AuditStatusEntry {
                    indicator,
                    has_session,
                },
            );
        }
    }

    vec![Msg::AuditStatusLoaded(status)]
}

/// Maps a live tmux session name back to the ticket key it was launched for,
/// per [`crate::work::audit::audit_session_name`]'s `tm-audit-<lowercased
/// key>` convention: strips the `tm-audit-` prefix and uppercases the
/// remainder. `None` for any session name that doesn't start with that
/// prefix (an unrelated tmux session).
///
/// Round-trip assumption: Jira ticket keys are always uppercase
/// (`PROJ-123`), and `audit_session_name` only ever lowercases -- never
/// otherwise transforms -- the key, so uppercasing the stripped suffix
/// recovers the original key exactly.
fn audit_session_ticket_key(session_name: &str) -> Option<String> {
    session_name
        .strip_prefix("tm-audit-")
        .map(str::to_uppercase)
}

/// Run `Cmd::LaunchAudit`: launch a ticket-audit session for `key` via
/// [`crate::work::audit::launch_audit`], mapping the outcome to a
/// status-line message. `deps.store` being `None` surfaces as `runs db
/// unavailable` rather than attempting a launch that has nowhere to
/// pre-register a run row.
fn launch_audit_cmd(deps: &TuiDeps, key: &str) -> Vec<Msg> {
    let Some(store) = &deps.store else {
        return vec![Msg::AuditActionResult("runs db unavailable".to_string())];
    };

    let message = match crate::work::audit::launch_audit(
        store,
        deps.tmux.as_ref(),
        &deps.audit,
        &deps.home,
        key,
    ) {
        Ok(_) => format!("launched audit for {key} -- press a to attach"),
        Err(crate::work::audit::AuditLaunchError::AlreadyRunning { session_name }) => {
            format!("audit already running ({session_name}) -- press a to attach")
        }
        Err(err) => err.to_string(),
    };
    vec![Msg::AuditActionResult(message)]
}

/// Search for tickets matching `jql` and map them to
/// [`crate::tui::app::TicketSummary`]s. Shared by `Cmd::FetchTickets` and
/// `Cmd::FetchRankTickets`, which differ only in which `Msg` the result (or
/// error) becomes.
fn search_tickets(deps: &TuiDeps, jql: &str) -> Result<Vec<TicketSummary>, JiraError> {
    let result = deps.jira.search(jql)?;
    Ok(result
        .issues
        .into_iter()
        .map(|issue| to_ticket_summary(issue, &deps.base_url))
        .collect())
}

/// Run `Cmd::FetchTickets`: search for tickets matching `jql` and map them to
/// [`crate::tui::app::TicketSummary`]s.
fn fetch_tickets(deps: &TuiDeps, jql: &str) -> Vec<Msg> {
    match search_tickets(deps, jql) {
        Ok(tickets) => vec![Msg::TicketsLoaded(tickets)],
        Err(err) => vec![Msg::TicketsFailed(err.to_string())],
    }
}

/// Run `Cmd::FetchRankTickets`: search for the project's full ranked ticket
/// list for [`crate::tui::app::Screen::Rank`].
fn fetch_rank_tickets(deps: &TuiDeps, jql: &str) -> Vec<Msg> {
    match search_tickets(deps, jql) {
        Ok(tickets) => vec![Msg::RankTicketsLoaded(tickets)],
        Err(err) => vec![Msg::RankTicketsFailed(err.to_string())],
    }
}

/// Run `Cmd::RankTicket`: move `key` to its new position relative to
/// `anchor`, reporting a human-readable confirmation on success (e.g.
/// `Ranked PROJ-3 above PROJ-7`).
fn rank_ticket(deps: &TuiDeps, key: &str, anchor: RankAnchor) -> Vec<Msg> {
    let message = match &anchor {
        RankAnchor::Before(other) => format!("Ranked {key} above {other}"),
        RankAnchor::After(other) => format!("Ranked {key} below {other}"),
    };
    match deps.jira.rank(&[key.to_string()], anchor) {
        Ok(()) => vec![Msg::RankApplied(message)],
        Err(err) => vec![Msg::RankFailed(err.to_string())],
    }
}

/// Run `Cmd::FetchAssignableUsers`: list the users eligible for assignment in
/// `project`, for the filter picker.
fn fetch_assignable_users(deps: &TuiDeps, project: &str) -> Vec<Msg> {
    match deps.jira.assignable_users(project) {
        Ok(users) => vec![Msg::AssignableUsersLoaded(users)],
        Err(err) => vec![Msg::AssignableUsersFailed(err.to_string())],
    }
}

/// Run `Cmd::FetchTransitions`: list the workflow transitions available on
/// `key`.
fn fetch_transitions(deps: &TuiDeps, key: &str) -> Vec<Msg> {
    match deps.jira.transitions(key) {
        Ok(transitions) => vec![Msg::TransitionsLoaded(transitions)],
        Err(err) => vec![Msg::TransitionsFailed(err.to_string())],
    }
}

/// Run `Cmd::ApplyTransition`: apply the transition, then re-fetch the issue
/// to learn its resulting status (the transition endpoint itself returns no
/// body).
fn apply_transition(deps: &TuiDeps, key: &str, transition_id: &str) -> Vec<Msg> {
    if let Err(err) = deps.jira.transition(key, transition_id) {
        return vec![Msg::TransitionFailed(err.to_string())];
    }
    match deps.jira.get_issue(key) {
        Ok(issue) => vec![Msg::TransitionApplied {
            key: key.to_string(),
            status_category: issue.fields.status.status_category.key.clone(),
            status: issue.fields.status.name,
        }],
        Err(err) => vec![Msg::TransitionFailed(err.to_string())],
    }
}

/// Best-effort open `url` in the user's default browser via the `open`
/// command. Failures are not surfaced as a dedicated message (the fixed `Msg`
/// set has no `OpenUrlFailed` variant); [`Msg::TicketsFailed`] is reused
/// purely for its `status_line`-setting effect, which is exactly what an
/// open-browser failure needs.
fn open_url(url: &str) -> Vec<Msg> {
    match std::process::Command::new("open").arg(url).status() {
        Ok(status) if status.success() => Vec::new(),
        Ok(status) => vec![Msg::TicketsFailed(format!(
            "failed to open browser (exit {status})"
        ))],
        Err(err) => vec![Msg::TicketsFailed(format!("failed to open browser: {err}"))],
    }
}

/// Convert a Jira [`Issue`] into a [`crate::tui::app::TicketSummary`],
/// deriving `url` from `base_url` and `description` from the issue's ADF
/// description via [`adf_to_text`].
fn to_ticket_summary(issue: Issue, base_url: &str) -> crate::tui::app::TicketSummary {
    let description = issue
        .fields
        .description
        .as_ref()
        .map(adf_to_text)
        .unwrap_or_default();
    let assignee = issue
        .fields
        .assignee
        .as_ref()
        .map(|a| a.display_name.clone());
    crate::tui::app::TicketSummary {
        url: format!("{base_url}/browse/{}", issue.key),
        key: issue.key,
        summary: issue.fields.summary,
        status_category: issue.fields.status.status_category.key.clone(),
        status: issue.fields.status.name,
        description,
        assignee,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jira::fake::FakeJiraClient;
    use crate::jira::jql::my_open_tickets_jql;
    use crate::jira::types::{IssueFields, JiraUser, Status, StatusCategory};

    fn issue(key: &str, status: &str) -> Issue {
        Issue {
            key: key.to_string(),
            fields: IssueFields {
                summary: "Fix the thing".to_string(),
                status: Status {
                    name: status.to_string(),
                    status_category: StatusCategory {
                        key: "new".to_string(),
                    },
                },
                description: Some(serde_json::json!({
                    "type": "doc",
                    "version": 1,
                    "content": [
                        { "type": "paragraph", "content": [{ "type": "text", "text": "Body text" }] }
                    ]
                })),
                assignee: None,
                issue_links: vec![],
            },
        }
    }

    /// A minimal in-memory terminal for tests that need to call [`run_cmds`]
    /// (which requires `&mut Terminal` for [`Cmd::AttachAudit`]'s
    /// suspend/restore, even though most tests never exercise that path).
    fn test_terminal() -> Terminal<ratatui::backend::TestBackend> {
        Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal should build")
    }

    fn deps(jira: FakeJiraClient) -> TuiDeps {
        TuiDeps {
            jira: Box::new(jira),
            base_url: "https://example.atlassian.net".to_string(),
            project_key: "PROJ".to_string(),
            board_column_order: Vec::new(),
            store: None,
            tmux: Box::new(crate::work::tmux::FakeTmuxOps::new()),
            audit: crate::config::AuditConfig::default(),
            home: std::path::PathBuf::from("/home/test"),
        }
    }

    #[test]
    fn fetch_tickets_maps_issues_to_ticket_summaries() {
        use crate::jira::types::SearchResult;

        let jira = FakeJiraClient::new().with_search_result(SearchResult {
            issues: vec![issue("PROJ-1", "To Do")],
            next_page_token: None,
        });
        let msgs = fetch_tickets(&deps(jira), &my_open_tickets_jql());
        match msgs.as_slice() {
            [Msg::TicketsLoaded(tickets)] => {
                assert_eq!(tickets.len(), 1);
                assert_eq!(tickets[0].key, "PROJ-1");
                assert_eq!(tickets[0].status, "To Do");
                assert_eq!(
                    tickets[0].url,
                    "https://example.atlassian.net/browse/PROJ-1"
                );
                assert_eq!(tickets[0].description, "Body text");
            }
            other => panic!("expected TicketsLoaded, got {other:?}"),
        }
    }

    #[test]
    fn fetch_tickets_with_empty_search_result_loads_empty_list() {
        let jira = FakeJiraClient::new();
        let msgs = fetch_tickets(&deps(jira), &my_open_tickets_jql());
        assert_eq!(msgs, vec![Msg::TicketsLoaded(vec![])]);
    }

    #[test]
    fn fetch_tickets_failure_emits_tickets_failed() {
        let jira = FakeJiraClient::new().with_search_error(500, "boom");
        let msgs = fetch_tickets(&deps(jira), &my_open_tickets_jql());
        match msgs.as_slice() {
            [Msg::TicketsFailed(message)] => assert_eq!(message, "Jira API error (500): boom"),
            other => panic!("expected TicketsFailed, got {other:?}"),
        }
    }

    #[test]
    fn fetch_assignable_users_success_emits_loaded() {
        let jira = FakeJiraClient::new().with_assignable_users(
            "PROJ",
            vec![JiraUser {
                account_id: "acct-1".to_string(),
                display_name: "Jane Doe".to_string(),
            }],
        );
        let msgs = fetch_assignable_users(&deps(jira), "PROJ");
        assert_eq!(
            msgs,
            vec![Msg::AssignableUsersLoaded(vec![JiraUser {
                account_id: "acct-1".to_string(),
                display_name: "Jane Doe".to_string(),
            }])]
        );
    }

    #[test]
    fn fetch_assignable_users_failure_emits_failed() {
        let jira = FakeJiraClient::new().with_assignable_users_error("PROJ", 500, "boom");
        let msgs = fetch_assignable_users(&deps(jira), "PROJ");
        match msgs.as_slice() {
            [Msg::AssignableUsersFailed(message)] => {
                assert_eq!(message, "Jira API error (500): boom")
            }
            other => panic!("expected AssignableUsersFailed, got {other:?}"),
        }
    }

    #[test]
    fn fetch_transitions_success_emits_transitions_loaded() {
        let jira = FakeJiraClient::new();
        let msgs = fetch_transitions(&deps(jira), "PROJ-1");
        assert_eq!(msgs, vec![Msg::TransitionsLoaded(vec![])]);
    }

    #[test]
    fn apply_transition_success_refetches_issue_for_new_status() {
        let jira = FakeJiraClient::new().with_issue("PROJ-1", issue("PROJ-1", "In Progress"));
        let msgs = apply_transition(&deps(jira), "PROJ-1", "11");
        assert_eq!(
            msgs,
            vec![Msg::TransitionApplied {
                key: "PROJ-1".to_string(),
                status: "In Progress".to_string(),
                status_category: "new".to_string()
            }]
        );
    }

    #[test]
    fn apply_transition_failure_to_refetch_emits_transition_failed() {
        let jira = FakeJiraClient::new().with_issue_not_found("PROJ-1");
        let msgs = apply_transition(&deps(jira), "PROJ-1", "11");
        match msgs.as_slice() {
            [Msg::TransitionFailed(_)] => {}
            other => panic!("expected TransitionFailed, got {other:?}"),
        }
    }

    #[test]
    fn to_ticket_summary_derives_url_and_extracts_description() {
        let summary = to_ticket_summary(issue("PROJ-1", "To Do"), "https://example.atlassian.net");
        assert_eq!(summary.key, "PROJ-1");
        assert_eq!(summary.status, "To Do");
        assert_eq!(summary.url, "https://example.atlassian.net/browse/PROJ-1");
        assert_eq!(summary.description, "Body text");
    }

    #[test]
    fn to_ticket_summary_with_no_description_is_empty_string() {
        let mut issue = issue("PROJ-1", "To Do");
        issue.fields.description = None;
        let summary = to_ticket_summary(issue, "https://example.atlassian.net");
        assert_eq!(summary.description, "");
    }

    #[test]
    fn to_ticket_summary_with_no_assignee_is_none() {
        let summary = to_ticket_summary(issue("PROJ-1", "To Do"), "https://example.atlassian.net");
        assert_eq!(summary.assignee, None);
    }

    #[test]
    fn to_ticket_summary_with_assignee_extracts_display_name() {
        use crate::jira::types::UserRef;

        let mut issue = issue("PROJ-1", "To Do");
        issue.fields.assignee = Some(UserRef {
            account_id: "acct-1".to_string(),
            display_name: "Jane Doe".to_string(),
        });
        let summary = to_ticket_summary(issue, "https://example.atlassian.net");
        assert_eq!(summary.assignee, Some("Jane Doe".to_string()));
    }

    #[test]
    fn fetch_rank_tickets_maps_issues_to_ticket_summaries() {
        use crate::jira::jql::ranked_tickets_jql;
        use crate::jira::types::SearchResult;

        let jira = FakeJiraClient::new().with_search_result(SearchResult {
            issues: vec![issue("PROJ-1", "To Do")],
            next_page_token: None,
        });
        let msgs = fetch_rank_tickets(&deps(jira), &ranked_tickets_jql("PROJ"));
        match msgs.as_slice() {
            [Msg::RankTicketsLoaded(tickets)] => {
                assert_eq!(tickets.len(), 1);
                assert_eq!(tickets[0].key, "PROJ-1");
            }
            other => panic!("expected RankTicketsLoaded, got {other:?}"),
        }
    }

    #[test]
    fn fetch_rank_tickets_failure_emits_rank_tickets_failed() {
        use crate::jira::jql::ranked_tickets_jql;

        let jira = FakeJiraClient::new().with_search_error(500, "boom");
        let msgs = fetch_rank_tickets(&deps(jira), &ranked_tickets_jql("PROJ"));
        match msgs.as_slice() {
            [Msg::RankTicketsFailed(message)] => assert_eq!(message, "Jira API error (500): boom"),
            other => panic!("expected RankTicketsFailed, got {other:?}"),
        }
    }

    #[test]
    fn rank_ticket_before_emits_rank_applied_with_above_message() {
        let jira = FakeJiraClient::new();
        let msgs = rank_ticket(
            &deps(jira),
            "PROJ-3",
            RankAnchor::Before("PROJ-7".to_string()),
        );
        assert_eq!(
            msgs,
            vec![Msg::RankApplied("Ranked PROJ-3 above PROJ-7".to_string())]
        );
    }

    #[test]
    fn rank_ticket_after_emits_rank_applied_with_below_message() {
        let jira = FakeJiraClient::new();
        let msgs = rank_ticket(
            &deps(jira),
            "PROJ-3",
            RankAnchor::After("PROJ-7".to_string()),
        );
        assert_eq!(
            msgs,
            vec![Msg::RankApplied("Ranked PROJ-3 below PROJ-7".to_string())]
        );
    }

    #[test]
    fn rank_ticket_failure_emits_rank_failed() {
        let jira = FakeJiraClient::new().with_rank_error(500, "boom");
        let msgs = rank_ticket(
            &deps(jira),
            "PROJ-3",
            RankAnchor::Before("PROJ-7".to_string()),
        );
        match msgs.as_slice() {
            [Msg::RankFailed(message)] => assert_eq!(message, "Jira API error (500): boom"),
            other => panic!("expected RankFailed, got {other:?}"),
        }
    }

    #[test]
    fn run_cmds_feeds_tickets_loaded_back_through_update() {
        use crate::jira::types::SearchResult;

        let jira = FakeJiraClient::new().with_search_result(SearchResult {
            issues: vec![issue("PROJ-1", "To Do")],
            next_page_token: None,
        });
        let mut terminal = test_terminal();
        let app = run_cmds(
            App::new(),
            vec![Cmd::FetchTickets {
                jql: my_open_tickets_jql(),
            }],
            &deps(jira),
            &mut terminal,
        );
        assert_eq!(app.columns.len(), 1);
        assert_eq!(app.selected_col, 0);
        assert_eq!(app.selected_row, 0);
    }

    fn watch_deps(store: crate::runs::RunStore) -> WatchDeps {
        WatchDeps { store }
    }

    fn start_params(ticket: &str) -> crate::runs::StartRun {
        crate::runs::StartRun {
            ticket: ticket.to_string(),
            lane: "backend".to_string(),
            worktree: "/tmp/wt".to_string(),
            branch: None,
            pid: None,
            kind: "lane".to_string(),
        }
    }

    #[test]
    fn execute_watch_load_runs_maps_summaries_to_cards() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();
        store.start_run(&start_params("PROJ-1")).unwrap();

        let msgs = execute_watch(&watch_deps(store), Cmd::LoadRuns);
        match msgs.as_slice() {
            [Msg::RunsLoaded(cards)] => {
                assert_eq!(cards.len(), 1);
                assert_eq!(cards[0].ticket, "PROJ-1");
                assert_eq!(cards[0].status, crate::runs::RunStatus::Running);
            }
            other => panic!("expected RunsLoaded, got {other:?}"),
        }
    }

    #[test]
    fn execute_watch_load_run_detail_for_existing_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();
        let run_id = store.start_run(&start_params("PROJ-1")).unwrap();
        store.add_event(run_id, "tool_use", None).unwrap();

        let msgs = execute_watch(&watch_deps(store), Cmd::LoadRunDetail { run_id });
        match msgs.as_slice() {
            [Msg::RunDetailLoaded(detail)] => {
                assert_eq!(detail.id, run_id);
                assert_eq!(detail.ticket, "PROJ-1");
                assert_eq!(detail.events.len(), 1);
                assert_eq!(detail.events[0].kind, "tool_use");
            }
            other => panic!("expected RunDetailLoaded, got {other:?}"),
        }
    }

    #[test]
    fn execute_watch_load_run_detail_populates_tool_counts() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();
        let run_id = store.start_run(&start_params("PROJ-1")).unwrap();
        store
            .add_event(run_id, "tool", Some(r#"{"tool":"Bash"}"#))
            .unwrap();
        store
            .add_event(run_id, "tool", Some(r#"{"tool":"Bash"}"#))
            .unwrap();
        store
            .add_event(run_id, "tool", Some(r#"{"tool":"Edit"}"#))
            .unwrap();

        let msgs = execute_watch(&watch_deps(store), Cmd::LoadRunDetail { run_id });
        match msgs.as_slice() {
            [Msg::RunDetailLoaded(detail)] => {
                assert_eq!(
                    detail.tool_counts,
                    vec![("Bash".to_string(), 2), ("Edit".to_string(), 1)]
                );
            }
            other => panic!("expected RunDetailLoaded, got {other:?}"),
        }
    }

    #[test]
    fn execute_watch_load_run_detail_prefers_authoritative_model_usage() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();
        let run_id = store.start_run(&start_params("PROJ-1")).unwrap();
        store
            .add_event(
                run_id,
                "usage",
                Some(r#"{"models":{"claude-fable-5":{"outputTokens":1}}}"#),
            )
            .unwrap();
        store
            .finish_run(
                run_id,
                &crate::runs::FinishRun {
                    status: crate::runs::RunStatus::Done,
                    model_usage: Some(
                        r#"{"claude-fable-5":{"outputTokens":58564,"costUSD":12.996}}"#.to_string(),
                    ),
                    ..crate::runs::FinishRun::default()
                },
            )
            .unwrap();

        let msgs = execute_watch(&watch_deps(store), Cmd::LoadRunDetail { run_id });
        match msgs.as_slice() {
            [Msg::RunDetailLoaded(detail)] => {
                let usage = detail.model_usage.as_ref().expect("expected model usage");
                assert_eq!(usage.label, "Model usage");
                assert!(usage.lines.iter().any(|l| l.contains("$13.00")));
            }
            other => panic!("expected RunDetailLoaded, got {other:?}"),
        }
    }

    #[test]
    fn execute_watch_load_run_detail_falls_back_to_live_usage_while_running() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();
        let run_id = store.start_run(&start_params("PROJ-1")).unwrap();
        store
            .add_event(
                run_id,
                "usage",
                Some(r#"{"models":{"claude-fable-5":{"outputTokens":58564}}}"#),
            )
            .unwrap();

        let msgs = execute_watch(&watch_deps(store), Cmd::LoadRunDetail { run_id });
        match msgs.as_slice() {
            [Msg::RunDetailLoaded(detail)] => {
                let usage = detail.model_usage.as_ref().expect("expected model usage");
                assert_eq!(usage.label, "Model usage (live)");
                assert!(usage.lines.iter().any(|l| l.contains("out 58.6k")));
            }
            other => panic!("expected RunDetailLoaded, got {other:?}"),
        }
    }

    #[test]
    fn execute_watch_load_run_detail_with_no_usage_has_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();
        let run_id = store.start_run(&start_params("PROJ-1")).unwrap();

        let msgs = execute_watch(&watch_deps(store), Cmd::LoadRunDetail { run_id });
        match msgs.as_slice() {
            [Msg::RunDetailLoaded(detail)] => {
                assert!(detail.model_usage.is_none());
            }
            other => panic!("expected RunDetailLoaded, got {other:?}"),
        }
    }

    #[test]
    fn execute_watch_load_run_detail_populates_agent_usage() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();
        let run_id = store.start_run(&start_params("PROJ-1")).unwrap();
        store
            .add_event(
                run_id,
                "agent_usage",
                Some(
                    r#"{"agentType":"elixir-implementer","description":"Implement thing",
                    "model":"claude-sonnet-5","outputTokens":1143,"inputTokens":2,
                    "cacheReadInputTokens":87519,"cacheCreationInputTokens":3012,
                    "totalToolUseCount":38,"durationMs":193659}"#,
                ),
            )
            .unwrap();

        let msgs = execute_watch(&watch_deps(store), Cmd::LoadRunDetail { run_id });
        match msgs.as_slice() {
            [Msg::RunDetailLoaded(detail)] => {
                assert_eq!(detail.agent_usage.len(), 1);
                assert!(detail.agent_usage[0].contains("elixir-implementer"));
            }
            other => panic!("expected RunDetailLoaded, got {other:?}"),
        }
    }

    #[test]
    fn execute_watch_load_run_detail_with_no_agent_usage_events_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();
        let run_id = store.start_run(&start_params("PROJ-1")).unwrap();

        let msgs = execute_watch(&watch_deps(store), Cmd::LoadRunDetail { run_id });
        match msgs.as_slice() {
            [Msg::RunDetailLoaded(detail)] => {
                assert!(detail.agent_usage.is_empty());
            }
            other => panic!("expected RunDetailLoaded, got {other:?}"),
        }
    }

    #[test]
    fn execute_watch_load_run_detail_for_missing_id_fails() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();

        let msgs = execute_watch(&watch_deps(store), Cmd::LoadRunDetail { run_id: 999 });
        match msgs.as_slice() {
            [Msg::RunDetailFailed(_)] => {}
            other => panic!("expected RunDetailFailed, got {other:?}"),
        }
    }

    #[test]
    fn execute_watch_reap_runs_on_empty_store_reaps_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();

        let msgs = execute_watch(&watch_deps(store), Cmd::ReapRuns);
        assert_eq!(msgs, vec![Msg::RunsReaped(0)]);
    }

    // --- audit_session_ticket_key ---

    #[test]
    fn audit_session_ticket_key_strips_prefix_and_uppercases() {
        assert_eq!(
            audit_session_ticket_key("tm-audit-proj-123"),
            Some("PROJ-123".to_string())
        );
    }

    #[test]
    fn audit_session_ticket_key_returns_none_for_unrelated_session() {
        assert_eq!(audit_session_ticket_key("some-other-session"), None);
    }

    // --- Cmd::LoadAuditStatus / load_audit_status ---

    fn audit_start_params(ticket: &str) -> crate::runs::StartRun {
        crate::runs::StartRun {
            kind: "audit".to_string(),
            lane: "audit".to_string(),
            ..start_params(ticket)
        }
    }

    #[test]
    fn load_audit_status_with_no_store_yields_empty_map() {
        let mut deps = deps(FakeJiraClient::new());
        deps.store = None;
        let msgs = load_audit_status(&deps);
        assert_eq!(msgs, vec![Msg::AuditStatusLoaded(HashMap::new())]);
    }

    #[test]
    fn load_audit_status_marks_running_audit_with_matching_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();
        store.start_run(&audit_start_params("PROJ-1")).unwrap();

        let mut deps = deps(FakeJiraClient::new());
        deps.store = Some(store);
        deps.tmux = Box::new(
            crate::work::tmux::FakeTmuxOps::new().with_list_sessions(Ok(vec![
                crate::work::tmux::TmuxSession {
                    name: "tm-audit-proj-1".to_string(),
                    path: "/repo/axiom".to_string(),
                },
            ])),
        );

        let msgs = load_audit_status(&deps);
        match msgs.as_slice() {
            [Msg::AuditStatusLoaded(status)] => {
                let entry = status.get("PROJ-1").expect("PROJ-1 should have an entry");
                assert_eq!(entry.indicator, crate::tui::app::AuditIndicator::Running);
                assert!(entry.has_session);
            }
            other => panic!("expected AuditStatusLoaded, got {other:?}"),
        }
    }

    #[test]
    fn load_audit_status_marks_starting_when_session_exists_with_no_run() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();

        let mut deps = deps(FakeJiraClient::new());
        deps.store = Some(store);
        deps.tmux = Box::new(
            crate::work::tmux::FakeTmuxOps::new().with_list_sessions(Ok(vec![
                crate::work::tmux::TmuxSession {
                    name: "tm-audit-proj-2".to_string(),
                    path: "/repo/axiom".to_string(),
                },
            ])),
        );

        let msgs = load_audit_status(&deps);
        match msgs.as_slice() {
            [Msg::AuditStatusLoaded(status)] => {
                let entry = status.get("PROJ-2").expect("PROJ-2 should have an entry");
                assert_eq!(entry.indicator, crate::tui::app::AuditIndicator::Starting);
                assert!(entry.has_session);
            }
            other => panic!("expected AuditStatusLoaded, got {other:?}"),
        }
    }

    #[test]
    fn load_audit_status_omits_finished_run_with_no_live_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();
        let run_id = store.start_run(&audit_start_params("PROJ-3")).unwrap();
        store
            .finish_run(
                run_id,
                &crate::runs::FinishRun {
                    status: crate::runs::RunStatus::Done,
                    ..crate::runs::FinishRun::default()
                },
            )
            .unwrap();

        let mut deps = deps(FakeJiraClient::new());
        deps.store = Some(store);
        // No live `tm-audit-proj-3` session: `FakeTmuxOps::new()`'s default
        // `list_sessions` is `Ok(vec![])`.

        let msgs = load_audit_status(&deps);
        assert_eq!(msgs, vec![Msg::AuditStatusLoaded(HashMap::new())]);
    }

    // --- Cmd::LaunchAudit / launch_audit_cmd ---

    #[test]
    fn launch_audit_cmd_with_no_store_reports_unavailable() {
        let mut deps = deps(FakeJiraClient::new());
        deps.store = None;
        let msgs = launch_audit_cmd(&deps, "PROJ-1");
        assert_eq!(
            msgs,
            vec![Msg::AuditActionResult("runs db unavailable".to_string())]
        );
    }

    #[test]
    fn launch_audit_cmd_success_reports_launched_message() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();

        let mut deps = deps(FakeJiraClient::new());
        deps.store = Some(store);
        deps.audit = crate::config::AuditConfig {
            dir: Some("/repo/axiom".to_string()),
            prompt: None,
        };

        let msgs = launch_audit_cmd(&deps, "PROJ-1");
        assert_eq!(
            msgs,
            vec![Msg::AuditActionResult(
                "launched audit for PROJ-1 -- press a to attach".to_string()
            )]
        );
    }

    #[test]
    fn launch_audit_cmd_not_configured_surfaces_error_text() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();

        let mut deps = deps(FakeJiraClient::new());
        deps.store = Some(store);
        // `deps.audit` defaults to `AuditConfig::default()`: no `dir` set.

        let msgs = launch_audit_cmd(&deps, "PROJ-1");
        match msgs.as_slice() {
            [Msg::AuditActionResult(message)] => {
                assert!(message.contains("[work.audit].dir"));
            }
            other => panic!("expected AuditActionResult, got {other:?}"),
        }
    }

    #[test]
    fn launch_audit_cmd_already_running_reports_attach_hint() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();

        let mut deps = deps(FakeJiraClient::new());
        deps.store = Some(store);
        deps.audit = crate::config::AuditConfig {
            dir: Some("/repo/axiom".to_string()),
            prompt: None,
        };
        deps.tmux = Box::new(crate::work::tmux::FakeTmuxOps::new().with_has_session(Ok(true)));

        let msgs = launch_audit_cmd(&deps, "PROJ-1");
        assert_eq!(
            msgs,
            vec![Msg::AuditActionResult(
                "audit already running (tm-audit-proj-1) -- press a to attach".to_string()
            )]
        );
    }

    // --- Cmd::AttachAudit routing (run_cmds intercepts it before `execute`) ---

    #[test]
    fn run_cmds_intercepts_attach_audit_and_reports_status_line() {
        let d = deps(FakeJiraClient::new());
        let mut terminal = test_terminal();
        let app = run_cmds(
            App::new(),
            vec![Cmd::AttachAudit {
                session_name: "tm-audit-proj-1".to_string(),
            }],
            &d,
            &mut terminal,
        );
        assert_eq!(app.status_line, "detached from tm-audit-proj-1");
    }
}
