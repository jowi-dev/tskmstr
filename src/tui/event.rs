//! Terminal wiring: the only module in `tui` that touches a real terminal or
//! performs network/process I/O.
//!
//! [`run`] owns the event loop; [`execute`] is the thin translation from a
//! [`Cmd`] to the [`Msg`] it produces, kept separate so it can be unit tested
//! with [`crate::jira::fake::FakeJiraClient`] instead of a live Jira and a
//! real terminal.
//!
//! [`Cmd::AttachSession`] is the one exception to that split: attaching needs
//! `&mut Terminal` to suspend and restore the alternate screen around the
//! blocking `tmux attach-session` call, which `execute`'s signature has no
//! access to (and shouldn't grow one just for this). [`run_cmds`] intercepts
//! it before it ever reaches `execute`, handles the suspend/restore itself
//! (see [`attach_session`]), and feeds the result back through `update` as an
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

use crate::jira::client::RankAnchor;
use crate::ticketing::error::ProviderError;
use crate::ticketing::provider::{TicketProvider, TicketQuery};
use crate::ticketing::types::Issue;
use crate::tui::app::{
    App, AssignChoice, AuditStatusEntry, Cmd, Msg, TicketSummary, audit_indicator,
    bot_watch_indicator, lane_run_indicator,
};
use crate::tui::app::{query_for_filter, update};
use crate::tui::keymap::{RetroOverlay, map_key};
use crate::tui::launcher::LaneLauncher;
use crate::tui::ui::draw;
use crate::work::audit::AUDIT_WINDOW_NAME;
use crate::work::bugbot::CLEANUP_WINDOW_NAME;
use crate::work::tmux::{AttachOutcome, TmuxOps};

/// How long to wait for a key press between redraws.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Which of [`Screen::Retro`]'s overlays `app` currently has open, for
/// [`map_key`]'s `retro_overlay` parameter. The two `App` fields are mutually
/// exclusive by construction (see [`App::show_retro_note_entry`]'s doc
/// comment), so `show_retro_note_entry` is checked first without needing to
/// assert the other is unset.
///
/// [`Screen::Retro`]: crate::tui::app::Screen::Retro
/// [`App::show_retro_note_entry`]: crate::tui::app::App::show_retro_note_entry
fn retro_overlay_for(app: &App) -> RetroOverlay {
    if app.show_retro_note_entry {
        RetroOverlay::NoteEntry
    } else if app.show_retro_severity_picker {
        RetroOverlay::SeverityPicker
    } else {
        RetroOverlay::None
    }
}

/// How long [`resolve_pr_for_ticket`]'s `gh pr list` lookup (the `o` key's
/// PR-vs-Jira picker) waits before giving up and falling back to opening
/// Jira directly, via [`crate::github::gh_cli::GhCli::pr_list_bounded`].
///
/// 8s: comfortably above how long an ordinary `gh pr list` call takes against
/// GitHub's API (a few hundred milliseconds to low seconds), so a healthy
/// network never spuriously falls back to Jira; short enough that a dead
/// network or expired `gh` auth -- which otherwise hangs forever, the defect
/// this whole mechanism exists to fix -- can only ever freeze the board for a
/// single-digit number of seconds rather than indefinitely.
const PR_LOOKUP_TIMEOUT: Duration = Duration::from_secs(8);

/// Run `kind` of a `tm pr watch` poll loop, filtered on by
/// [`load_bot_watch_status`].
const REVIEW_WATCH_KIND: &str = "review-watch";

/// Run `kind` of a bugbot-cleanup session, filtered on by
/// [`load_cleanup_status`]. Matches
/// [`crate::work::bugbot::launch_cleanup`]'s own `kind`.
const CLEANUP_KIND: &str = "bugbot-cleanup";

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
    pub jira: Box<dyn TicketProvider>,
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
    /// Validated `[work.create]` settings; launching the ticket-creation
    /// session is disabled (a status-line error) when `dir` is unset. See
    /// [`crate::work::create::launch_create`].
    pub create: crate::config::CreateConfig,
    /// The user's home directory, used to expand a leading `~` in
    /// `audit.dir`/`create.dir`.
    pub home: std::path::PathBuf,
    /// Launcher used to spawn `tm work run <lane> <key>` for
    /// [`Cmd::LaunchLaneRun`] (see `docs/plans/board-lane-runs.md`). Boxed
    /// for the same trait+fake reason as `tmux`.
    pub launcher: Box<dyn LaneLauncher>,
    /// Validated `[work.review_watch]` settings, with its `[work.audit].dir`
    /// fallback already applied by [`crate::config::merge_work`]. Launching a
    /// cleanup session is disabled (a status-line error) when `dir` is unset;
    /// see [`crate::work::bugbot::launch_cleanup`].
    pub review_watch: crate::config::ReviewWatchConfig,
    /// `$XDG_DATA_HOME`, if set, used (with `home`) to locate the findings
    /// file a launched cleanup session's prompt points at.
    pub xdg_data_home: Option<std::path::PathBuf>,
    /// `config.work.lanes` keys, threaded into
    /// [`crate::tui::app::App::with_lane_names`] at construction (see that
    /// method's doc comment).
    pub lane_names: Vec<String>,
    /// Count of configured `[work.lanes]` entries hidden from `lane_names`
    /// because their repo's resolved backend identity doesn't match this
    /// repo's own (see [`crate::config::compatible_lane_names`]), threaded
    /// into [`crate::tui::app::App::with_hidden_lane_count`] at
    /// construction. See GitHub issue #5 phase 2:
    /// `docs/plans/issue-5-lane-backend-routing.md`.
    pub hidden_lane_count: usize,
    /// Whether `audit.dir` above was already redirected from
    /// `[work.audit].dir`'s configured value to the current repo (`cwd`)
    /// because the configured dir's resolved backend identity didn't match
    /// this repo's own (see [`crate::config::resolve_audit_host_dir`]).
    /// [`launch_audit_cmd`] notes this in its status-line message when
    /// `true`, per the plan's "surface the fallback in the status line"
    /// decision.
    pub audit_dir_fallback: bool,
    /// Whether `create.dir` above was already redirected from
    /// `[work.create].dir`'s configured value to the current repo (`cwd`)
    /// because the configured dir's resolved backend identity didn't match
    /// this repo's own — the same
    /// [`resolve_audit_host_dir`](crate::config::resolve_audit_host_dir)
    /// treatment `audit_dir_fallback` reports, applied to the create dir so
    /// an in-session `tm ticket create` files against the board's own
    /// backend. [`launch_create_and_attach`] notes this in its status-line
    /// message when `true`.
    pub create_dir_fallback: bool,
    /// `gh` CLI wrapper used by [`Cmd::ResolvePrForTicket`] to list a
    /// ticket's repo's open pull requests.
    pub gh: Box<dyn crate::github::gh_cli::GhCli>,
    /// `git` operations used by [`resolve_repo_root_for_pr_lookup`]'s
    /// `resolve_watch_repo_root` fallback (`git rev-parse` the repo root of
    /// `cwd`) when a ticket has no lane run to resolve a repo from.
    pub git: Box<dyn crate::work::git::GitOps>,
    /// The board process's working directory, passed to
    /// [`resolve_repo_root_for_pr_lookup`] as the `cwd` a lane-less ticket's
    /// PR lookup falls back to resolving a repo root from.
    pub cwd: std::path::PathBuf,
    /// `config.work.lanes`, kept in full (not just names, unlike
    /// `lane_names`) so [`resolve_repo_root_for_pr_lookup`] can look up a
    /// lane's `repo` the same way `tm pr watch`'s
    /// `crate::cli::pr::resolve_watch_repo_root` does.
    pub lanes: std::collections::BTreeMap<String, crate::config::LaneConfig>,
    /// The board's own repo's resolved backend identity (the same value
    /// issue #5's lane-compatibility filtering derives `lane_names` from).
    /// Its [`session_slug`](crate::config::BackendIdentity::session_slug)
    /// qualifies every ticket-session name the board builds or recognizes,
    /// so same-numbered tickets in different repos never alias (GitHub
    /// issue #10).
    pub backend_identity: crate::config::BackendIdentity,
}

/// One board-launched child (`tm work run`, `tm pr watch`, or `tm review
/// fix`) that hasn't yet reported completion, tracked by [`run`]'s event loop
/// between [`run_cmds`]'s [`Cmd::LaunchLaneRun`]/[`Cmd::LaunchBotWatch`]/
/// [`Cmd::LaunchReviewFix`] interception (which creates the entry) and
/// [`poll_pending_launches`] (which removes it once
/// [`crate::tui::launcher::LaunchHandle::try_finish`] resolves).
struct PendingLaunch {
    /// The ticket key the launch was for, echoed back in the result `Msg`.
    key: String,
    /// Which `Msg` this entry's completion reports as.
    kind: PendingLaunchKind,
    /// The in-flight launcher child.
    handle: Box<dyn crate::tui::launcher::LaunchHandle>,
}

/// What a [`PendingLaunch`] was spawned for, which is all
/// [`poll_pending_launches`] needs in order to pick the right result `Msg`.
/// Both kinds share one launcher trait and one registry -- they differ only in
/// argv and in which `Msg` their completion feeds back through `update`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingLaunchKind {
    /// `tm work run <lane> <key>`; reports [`Msg::LaneRunLaunchResult`].
    LaneRun,
    /// `tm pr watch <key>`; reports [`Msg::BotWatchLaunchResult`].
    BotWatch,
    /// `tm review fix <key>`; reports [`Msg::ReviewFixLaunchResult`].
    ReviewFix,
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
/// and every badge status, then loops: draw the current screen, wait up to
/// `POLL_INTERVAL` for a key press. A key press maps to a [`Msg`] which runs
/// through [`crate::tui::app::update`], executing any resulting [`Cmd`]s; a
/// timed-out poll feeds [`Msg::Tick`] through the same path (mirroring
/// [`run_watch`]), which is what drives [`Screen::Board`]'s periodic badge
/// polling ([`Cmd::LoadAuditStatus`], [`Cmd::LoadLaneRunStatus`],
/// [`Cmd::LoadBotWatchStatus`] and [`Cmd::LoadCleanupStatus`]). Every
/// iteration also polls `launches` (the board-launched lane runs still in
/// flight, per [`run_cmds`]'s [`Cmd::LaunchLaneRun`] interception) via
/// [`poll_pending_launches`], feeding each completion's
/// [`Msg::LaneRunLaunchResult`] through `update` the same way a key press or
/// tick would. The terminal is always restored before returning, including
/// on error.
///
/// [`Screen::Board`]: crate::tui::app::Screen::Board
pub fn run(deps: TuiDeps) -> Result<(), TuiError> {
    enable_raw_mode()?;
    let _guard = TerminalGuard;
    execute!(std::io::stdout(), EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut launches: Vec<PendingLaunch> = Vec::new();

    let mut app = App {
        project_key: deps.project_key.clone(),
        session_slug: deps.backend_identity.session_slug(),
        board_column_order: deps.board_column_order.clone(),
        ..App::new()
    }
    .with_lane_names(deps.lane_names.clone())
    .with_hidden_lane_count(deps.hidden_lane_count);
    let query = query_for_filter(&app.filter, &app.project_key);
    app = run_cmds(
        app,
        vec![
            Cmd::FetchTickets { query },
            Cmd::LoadAuditStatus,
            Cmd::LoadLaneRunStatus,
            Cmd::LoadBotWatchStatus,
            Cmd::LoadCleanupStatus,
        ],
        &deps,
        &mut terminal,
        &mut launches,
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
                    app.show_assign_picker,
                    app.show_lane_picker,
                    app.show_browser_picker,
                    app.is_rank_grabbed(),
                    app.show_run_detail,
                    retro_overlay_for(&app),
                    key_event.code,
                )
            {
                let (next_app, cmds) = update(app, msg);
                app = run_cmds(next_app, cmds, &deps, &mut terminal, &mut launches);
            }
        } else {
            let (next_app, cmds) = update(app, Msg::Tick);
            app = run_cmds(next_app, cmds, &deps, &mut terminal, &mut launches);
        }

        for msg in poll_pending_launches(&mut launches) {
            let (next_app, cmds) = update(app, msg);
            app = run_cmds(next_app, cmds, &deps, &mut terminal, &mut launches);
        }
    }

    Ok(())
}

/// Execute every `Cmd` in `cmds`, feeding each resulting `Msg` back through
/// `update` (which may itself produce further `Cmd`s, e.g. loading
/// transitions after opening the detail screen).
///
/// [`Cmd::AttachSession`] is intercepted here rather than passed to `execute`:
/// see the module docs. [`Cmd::LaunchLaneRun`]/[`Cmd::LaunchBotWatch`]/
/// [`Cmd::LaunchReviewFix`] are intercepted for the same kind of reason --
/// they need mutable access to `launches`, the in-flight launcher registry,
/// which `execute`'s signature has no access to. A spawn failure feeds an
/// immediate launch-result `Msg` error straight through `update`; a spawn
/// success instead pushes a [`PendingLaunch`] onto `launches` for
/// [`poll_pending_launches`] to resolve later, once its
/// [`crate::tui::launcher::LaunchHandle::try_finish`] reports completion.
/// [`Cmd::ViewDiff`] is intercepted like [`Cmd::AttachSession`] (needs `&mut
/// Terminal` to suspend/restore around the blocking `vdiff` call -- see
/// [`view_diff`]). [`Cmd::ResolvePrForTicket`] is intercepted for yet
/// another reason: it needs `&mut Terminal` to force a status-line redraw
/// before its blocking `gh pr list` call, so the "resolving PR for
/// <key>..." message set by `update`'s `open_browser_action` is actually on
/// screen for the (bounded) wait rather than only appearing after it -- see
/// [`resolve_pr_for_ticket`].
///
/// Generic over the terminal backend (rather than fixed to
/// [`CrosstermBackend`]) purely so tests can drive it with
/// `ratatui::backend::TestBackend` instead of a real terminal.
fn run_cmds<B: Backend>(
    mut app: App,
    cmds: Vec<Cmd>,
    deps: &TuiDeps,
    terminal: &mut Terminal<B>,
    launches: &mut Vec<PendingLaunch>,
) -> App {
    let mut pending: VecDeque<Cmd> = cmds.into();
    while let Some(cmd) = pending.pop_front() {
        if let Cmd::AttachSession { session_name } = cmd {
            // One result `Msg` for all three keys that attach (`a`, `b`, `s`):
            // the Cmd is "attach to this ticket's session" in every case, and
            // only its status line differs. `Msg::AuditActionResult` stays for
            // *launch* outcomes, which are audit-specific.
            let message = attach_session(terminal, deps.tmux.as_ref(), &session_name);
            let (next_app, more_cmds) = update(app, Msg::SessionAttachResult(message));
            app = next_app;
            pending.extend(more_cmds);
            continue;
        }
        if let Cmd::LaunchCreate = cmd {
            // Launch-then-attach in one keypress (issue #15): the launch
            // itself is `execute`-shaped, but the immediate attach needs
            // `&mut Terminal`, so the whole command is intercepted here like
            // `Cmd::AttachSession`.
            let message = launch_create_and_attach(terminal, deps);
            let (next_app, more_cmds) = update(app, Msg::CreateActionResult(message));
            app = next_app;
            pending.extend(more_cmds);
            continue;
        }
        if let Cmd::LaunchLaneRun { lane, key } = cmd {
            let argv = lane_run_argv(&lane, &key);
            let (next_app, more_cmds) =
                spawn_watched_child(app, deps, launches, key, PendingLaunchKind::LaneRun, &argv);
            app = next_app;
            pending.extend(more_cmds);
            continue;
        }
        if let Cmd::LaunchBotWatch { key } = cmd {
            let argv = bot_watch_argv(&key);
            let (next_app, more_cmds) =
                spawn_watched_child(app, deps, launches, key, PendingLaunchKind::BotWatch, &argv);
            app = next_app;
            pending.extend(more_cmds);
            continue;
        }
        if let Cmd::ViewLogs { key } = cmd {
            let scope = deps.backend_identity.scope();
            let message = view_logs(
                terminal,
                deps.store.as_ref(),
                Some(&scope),
                &deps.home,
                &key,
            );
            let (next_app, more_cmds) = update(app, Msg::LogsActionResult(message));
            app = next_app;
            pending.extend(more_cmds);
            continue;
        }
        if let Cmd::ViewDiff { key } = cmd {
            let scope = deps.backend_identity.scope();
            let message = view_diff(terminal, deps.store.as_ref(), Some(&scope), &key);
            let (next_app, more_cmds) = update(app, Msg::DiffActionResult(message));
            app = next_app;
            pending.extend(more_cmds);
            continue;
        }
        if let Cmd::LaunchReviewFix { key } = cmd {
            let argv = review_fix_argv(&key);
            let (next_app, more_cmds) = spawn_watched_child(
                app,
                deps,
                launches,
                key,
                PendingLaunchKind::ReviewFix,
                &argv,
            );
            app = next_app;
            pending.extend(more_cmds);
            continue;
        }
        if let Cmd::ResolvePrForTicket { key, jira_url } = cmd {
            // Unlike every other `Cmd`, this one needs `&mut Terminal` for a
            // reason none of the above do: not to suspend the alternate
            // screen, but to force a redraw *before* the blocking `gh pr
            // list` call runs. `update`'s `open_browser_action` already set
            // `app.status_line` to `resolving PR for <key>...`, but this
            // loop's ordinary structure only calls `terminal.draw` at the
            // *top* of `run`'s `while` loop -- after every `Cmd` from the
            // current keypress has finished executing (see `run`'s doc
            // comment). Without this explicit draw here, that status-line
            // message would never reach the screen before the lookup
            // started, and the board would look just as hung as it did
            // before this fix, even though the wait is now bounded.
            let _ = terminal.draw(|frame| draw(frame, &app));
            for msg in resolve_pr_for_ticket(deps, key, jira_url) {
                let (next_app, more_cmds) = update(app, msg);
                app = next_app;
                pending.extend(more_cmds);
            }
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

/// The argv [`Cmd::LaunchLaneRun`] spawns through
/// [`crate::tui::launcher::LaneLauncher::spawn`]: `tm work run <lane> <key>`,
/// as owned `String`s (the trait takes a general argv, not a lane/key pair,
/// so it can also spawn `tm pr watch <key>`).
fn lane_run_argv(lane: &str, key: &str) -> Vec<String> {
    vec![
        "work".to_string(),
        "run".to_string(),
        lane.to_string(),
        key.to_string(),
    ]
}

/// The argv [`Cmd::LaunchBotWatch`] spawns through
/// [`crate::tui::launcher::LaneLauncher::spawn`]: `tm pr watch <key>`, whose
/// own quick resolve-then-detach step (see `docs/plans/bugbot-watch.md`'s "CLI
/// surface") is what makes it watchable in the same way `tm work run`'s
/// preflight is.
fn bot_watch_argv(key: &str) -> Vec<String> {
    vec!["pr".to_string(), "watch".to_string(), key.to_string()]
}

/// The argv [`Cmd::LaunchReviewFix`] spawns through
/// [`crate::tui::launcher::LaneLauncher::spawn`]: `tm review fix <key>`, the
/// fixed contract agreed in `docs/plans/board-vdiff-review-loop.md`'s
/// "Decisions" section between this board half and `tm review fix`'s own
/// (concurrently developed) CLI half.
fn review_fix_argv(key: &str) -> Vec<String> {
    vec!["review".to_string(), "fix".to_string(), key.to_string()]
}

/// Spawn `argv` as a watched child for `key`, registering it in `launches` on
/// success. A spawn failure never reaches the registry: it feeds the matching
/// launch-result `Msg` (an `Err`) straight back through `update`, returning
/// any further `Cmd`s that produced. Shared by [`Cmd::LaunchLaneRun`] and
/// [`Cmd::LaunchBotWatch`], which differ only in argv and result `Msg`.
fn spawn_watched_child(
    app: App,
    deps: &TuiDeps,
    launches: &mut Vec<PendingLaunch>,
    key: String,
    kind: PendingLaunchKind,
    argv: &[String],
) -> (App, Vec<Cmd>) {
    match deps.launcher.spawn(argv) {
        Ok(handle) => {
            launches.push(PendingLaunch { key, kind, handle });
            (app, Vec::new())
        }
        Err(err) => update(app, launch_result_msg(kind, key, Err(err))),
    }
}

/// The result `Msg` a [`PendingLaunchKind`] reports its outcome as.
fn launch_result_msg(kind: PendingLaunchKind, key: String, result: Result<(), String>) -> Msg {
    match kind {
        PendingLaunchKind::LaneRun => Msg::LaneRunLaunchResult { key, result },
        PendingLaunchKind::BotWatch => Msg::BotWatchLaunchResult { key, result },
        PendingLaunchKind::ReviewFix => Msg::ReviewFixLaunchResult { key, result },
    }
}

/// Poll every entry in `launches` for completion (non-blocking, via
/// [`crate::tui::launcher::LaunchHandle::try_finish`]), removing each one
/// that finishes and returning its [`Msg::LaneRunLaunchResult`]. Entries
/// still in flight are left in `launches` untouched. Pure with respect to
/// `App` -- it only mutates the registry and returns `Msg`s -- so it's
/// tested here without a real event loop; [`run`] feeds the returned `Msg`s
/// through `update` itself.
fn poll_pending_launches(launches: &mut Vec<PendingLaunch>) -> Vec<Msg> {
    let mut msgs = Vec::new();
    launches.retain_mut(|pending| match pending.handle.try_finish() {
        Some(result) => {
            msgs.push(launch_result_msg(pending.kind, pending.key.clone(), result));
            false
        }
        None => true,
    });
    msgs
}

/// Suspend the board's alternate screen and raw mode, run
/// [`TmuxOps::attach`] with inherited stdio — outside tmux that's a
/// blocking `tmux attach-session -t <session_name>` (until the user
/// detaches, e.g. `C-b d`, or the session ends); inside tmux it's a `tmux
/// switch-client -t <session_name>` that returns immediately while the
/// user's client is now showing the target session (issue #6) — then
/// restore the alternate screen and raw mode and clear the terminal so the
/// board redraws cleanly. In the switch case the board keeps running in its
/// own tmux window, ready for the user to jump back (`prefix + s`,
/// `switch-client -l`). Returns a status-line message describing the
/// outcome, worded per [`AttachOutcome`].
///
/// Shared by all three keys that attach: `a` (a live `audit` window), `b` (a
/// live `bugbot` window), and `s` (the ticket's session as such, issue #2
/// phase 5). They differ only in how they decide *which* session, which is
/// [`crate::tui::app`]'s job; the suspend/restore dance is identical, and is
/// the part that must not be reimplemented per key — getting the ordering
/// wrong strands the user's shell.
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
///    behind it), and the status line reads `detached from tm-<scope>-<key>`.
/// 4. Kill the tmux session from another terminal while attached (`tmux
///    kill-session -t tm-<scope>-<key>`); confirm `tmux attach-session`
///    exiting with an error still leaves this terminal fully usable (raw
///    mode re-enabled, alternate screen re-entered, board redrawn) rather
///    than stranding the shell.
/// 5. Press `s` on a ticket whose session holds a `work` window but no
///    `audit` one: it must still attach, where `a` would launch an audit.
/// 6. Press `s` on a ticket that has never been touched: no session exists,
///    so the status line reports the `tmux attach-session` failure and the
///    board must be fully usable afterwards (this is the same restore path
///    as step 4, reached without ever having attached).
/// 7. Run the board itself inside a tmux session and press `s` on a ticket
///    with a live session: the client must switch to that session (no
///    "sessions should be nested with care" refusal). Jump back to the
///    board's window (`prefix + s`): it must be redrawn cleanly with the
///    status line reading `switched client to tm-<scope>-<key>`.
fn attach_session<B: Backend>(
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
        Ok(AttachOutcome::Detached) => format!("detached from {session_name}"),
        Ok(AttachOutcome::Switched) => format!("switched client to {session_name}"),
        Err(err) => format!("attach to {session_name} failed: {err}"),
    }
}

/// Run [`Cmd::ViewLogs`]: resolve `key`'s latest run (any `kind`, same
/// resolution [`Msg::ViewRunAction`]'s overlay uses) via
/// [`crate::cli::runs::resolve_run`], then its log path via
/// [`crate::cli::runs::resolve_log_path`], and open it in `less`, suspending
/// and restoring the board's terminal state around the blocking call exactly
/// like [`attach_session`]. Every failure mode (no store, no run, no log path,
/// missing file, pager launch failure) is reported as a status-line message
/// rather than an error -- viewing a log is a convenience action, not
/// something that should be able to crash the board.
fn view_logs<B: Backend>(
    terminal: &mut Terminal<B>,
    store: Option<&crate::runs::RunStore>,
    scope: Option<&str>,
    home: &std::path::Path,
    key: &str,
) -> String {
    let Some(store) = store else {
        return "no run store available".to_string();
    };
    let run = match crate::cli::runs::resolve_run(store, scope, key, None) {
        Ok(run) => run,
        Err(_) => return format!("no runs recorded for {key}"),
    };
    let Some(path) = crate::cli::runs::resolve_log_path(&run, home) else {
        return format!("run {} for {key} has no log path", run.id);
    };
    if !path.exists() {
        return format!("log file {} does not exist", path.display());
    }

    let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
    let _ = disable_raw_mode();

    let result = std::process::Command::new("less")
        .arg("+G")
        .arg(&path)
        .status();

    let _ = enable_raw_mode();
    let _ = execute!(std::io::stdout(), EnterAlternateScreen);
    let _ = terminal.clear();

    match result {
        Ok(status) if status.success() => format!("viewed log for {key}"),
        Ok(status) => format!("less exited with {status}"),
        Err(err) => format!("failed to launch pager: {err}"),
    }
}

/// Resolves the worktree directory [`Cmd::ViewDiff`] should launch `vdiff`
/// in: `key`'s latest `kind = "lane"` run's `worktree` column, per
/// `docs/plans/board-vdiff-review-loop.md`'s "Decisions" section (`vdiff`
/// has no `--pr` flag and detects the base branch itself, so the run row's
/// recorded `worktree` is all that's needed -- no path reconstruction).
///
/// Pulled out of [`view_diff`] so this resolution logic -- unlike the actual
/// `vdiff` launch, which needs a real terminal and a real subprocess --
/// stays unit-testable (see this module's tests). Checks the worktree still
/// exists on disk before returning it: a run row can outlive `tm work
/// remove`, which deletes the worktree but not its run history.
fn resolve_vdiff_worktree(
    store: Option<&crate::runs::RunStore>,
    scope: Option<&str>,
    key: &str,
) -> Result<std::path::PathBuf, String> {
    let store = store.ok_or_else(|| "no run store available".to_string())?;
    let run = crate::cli::runs::resolve_run(store, scope, key, Some("lane"))
        .map_err(|_| format!("no lane run for {key}"))?;
    let worktree = std::path::PathBuf::from(&run.worktree);
    if !worktree.is_dir() {
        return Err(format!(
            "worktree {} for {key} no longer exists",
            run.worktree
        ));
    }
    Ok(worktree)
}

/// The executable [`view_diff`] launches. A bare name, looked up on `PATH`:
/// every flag belongs in [`VDIFF_ARGS`], since anything in here is taken
/// literally as the file to execute.
const VDIFF_PROGRAM: &str = "vdiff";

/// The flags [`view_diff`] launches [`VDIFF_PROGRAM`] with. `--tui` selects
/// vdiff's ratatui frontend over its default egui window: the board is
/// already a terminal application the user is driving over a TTY (possibly a
/// remote one), so handing the review off to a windowed GUI is the wrong
/// medium — and on a machine with no display, no medium at all.
const VDIFF_ARGS: [&str; 1] = ["--tui"];

/// Run [`Cmd::ViewDiff`]: resolve `key`'s lane-run worktree (via
/// [`resolve_vdiff_worktree`]) and open it in `vdiff`, suspending and
/// restoring the board's terminal state around the blocking call exactly
/// like [`view_logs`]/[`attach_session`] -- `vdiff` is an interactive GUI/TUI
/// that needs the real TTY, per
/// `docs/plans/board-vdiff-review-loop.md`'s "Decisions" section. No
/// `--pr`/PR-resolution flags are passed: `vdiff` detects the worktree's base
/// branch itself.
///
/// Every failure mode is a status-line message rather than an error: no run
/// store, no lane run, a worktree that's since been removed (all three from
/// [`resolve_vdiff_worktree`]), and -- the one case only reachable past the
/// suspend/restore dance -- `vdiff` missing from `PATH`, distinguished from
/// other spawn failures via [`std::io::ErrorKind::NotFound`] so it reads as
/// "not installed" rather than looking like the board hung.
///
/// ## What's unit-tested vs. deferred to manual verification
///
/// [`resolve_vdiff_worktree`]'s resolution logic is exercised directly in
/// this module's tests. The actual `vdiff` launch and its interaction with a
/// real terminal are not -- matching [`view_logs`]'s and
/// `crate::tui::launcher::RealLaunchHandle`'s existing carve-outs for this
/// class of mechanics. Verify manually per
/// `docs/plans/board-vdiff-review-loop.md`'s "Manual verification" section.
fn view_diff<B: Backend>(
    terminal: &mut Terminal<B>,
    store: Option<&crate::runs::RunStore>,
    scope: Option<&str>,
    key: &str,
) -> String {
    let worktree = match resolve_vdiff_worktree(store, scope, key) {
        Ok(worktree) => worktree,
        Err(message) => return message,
    };

    let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
    let _ = disable_raw_mode();

    let result = std::process::Command::new(VDIFF_PROGRAM)
        .args(VDIFF_ARGS)
        .current_dir(&worktree)
        .status();

    let _ = enable_raw_mode();
    let _ = execute!(std::io::stdout(), EnterAlternateScreen);
    let _ = terminal.clear();

    match result {
        Ok(status) if status.success() => format!("reviewed {key} in vdiff"),
        Ok(status) => format!("vdiff exited with {status}"),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            "vdiff not found on PATH".to_string()
        }
        Err(err) => format!("failed to launch vdiff: {err}"),
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
                    app.show_assign_picker,
                    app.show_lane_picker,
                    app.show_browser_picker,
                    app.is_rank_grabbed(),
                    app.show_run_detail,
                    RetroOverlay::None,
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
    load_run_detail_tail(&deps.store, run)
}

/// Run `Cmd::LoadTicketRunDetail`: resolve `key`'s latest run (any `kind`)
/// and load its full detail, for the run detail floating window opened from
/// the board. Unlike [`load_run_detail`], `deps.store` may be absent (a
/// broken runs DB must never block the Jira board), so that case gets its
/// own `Msg::RunDetailFailed` rather than the debug-assert `execute` uses for
/// truly unreachable `Cmd`s.
fn load_ticket_run_detail(deps: &TuiDeps, key: &str) -> Vec<Msg> {
    let Some(store) = &deps.store else {
        return vec![Msg::RunDetailFailed("run database unavailable".to_string())];
    };
    let run = match store.latest_run_for_ticket(Some(&deps.backend_identity.scope()), key) {
        Ok(Some(run)) => run,
        Ok(None) => return vec![Msg::RunDetailFailed(format!("no runs for {key}"))],
        Err(err) => return vec![Msg::RunDetailFailed(err.to_string())],
    };
    load_run_detail_tail(store, run)
}

/// Shared tail of [`load_run_detail`]/[`load_ticket_run_detail`]: given a
/// resolved [`crate::runs::Run`], load its event timeline and produce
/// [`Msg::RunDetailLoaded`], or [`Msg::RunDetailFailed`] if the events can't
/// be read.
fn load_run_detail_tail(store: &crate::runs::RunStore, run: crate::runs::Run) -> Vec<Msg> {
    let events = match store.events_for_run(run.id) {
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
        Cmd::FetchTickets { query } => fetch_tickets(deps, &query),
        Cmd::FetchAssignableUsers { project } => fetch_assignable_users(deps, &project),
        Cmd::FetchTransitions { key } => fetch_transitions(deps, &key),
        Cmd::ApplyTransition { key, transition_id } => apply_transition(deps, &key, &transition_id),
        Cmd::AssignTicket { key, choice } => assign_ticket_cmd(deps, &key, &choice),
        Cmd::OpenUrl(url) => open_url(&url),
        Cmd::FetchRankTickets { query } => fetch_rank_tickets(deps, &query),
        Cmd::RankTicket { key, anchor } => rank_ticket(deps, &key, anchor),
        Cmd::LoadAuditStatus => load_audit_status(deps),
        Cmd::LaunchAudit { key } => launch_audit_cmd(deps, &key),
        Cmd::LoadLaneRunStatus => load_lane_run_status(deps),
        Cmd::LoadBotWatchStatus => load_bot_watch_status(deps),
        Cmd::LoadCleanupStatus => load_cleanup_status(deps),
        Cmd::LaunchCleanup { key } => launch_cleanup_cmd(deps, &key),
        Cmd::LoadTicketRunDetail { key } => load_ticket_run_detail(deps, &key),
        Cmd::FetchRetroTickets { query } => fetch_retro_tickets(deps, &query),
        Cmd::RecordRetro {
            key,
            verdict,
            severity,
            notes,
        } => record_retro(deps, &key, verdict, severity, notes.as_deref()),
        // `Cmd::AttachSession`/`Cmd::ViewDiff` need `&mut Terminal` (to
        // suspend/restore the alternate screen around a blocking call --
        // `tmux attach`/`vdiff`, respectively); `Cmd::LaunchLaneRun`/
        // `Cmd::LaunchBotWatch`/`Cmd::LaunchReviewFix` need `&mut
        // Vec<PendingLaunch>` (the in-flight launcher registry);
        // `Cmd::ResolvePrForTicket` needs `&mut Terminal` too, to force a
        // redraw before its blocking (bounded) `gh pr list` call runs -- none
        // of which this function's signature has access to; `run_cmds`
        // always intercepts all of these before calling `execute` (see the
        // module docs), so they're unreachable here in practice.
        //
        // The Jira board never enters `Screen::Runs`, so `update` can never
        // produce one of the `Load*`/`Reap*` run-store `Cmd`s for
        // `run`/`execute` to handle either.
        other @ (Cmd::LoadRuns
        | Cmd::LoadRunDetail { .. }
        | Cmd::ReapRuns
        | Cmd::AttachSession { .. }
        | Cmd::LaunchCreate
        | Cmd::LaunchLaneRun { .. }
        | Cmd::LaunchBotWatch { .. }
        | Cmd::ViewLogs { .. }
        | Cmd::ViewDiff { .. }
        | Cmd::LaunchReviewFix { .. }
        | Cmd::ResolvePrForTicket { .. }) => {
            debug_assert!(
                false,
                "execute: unreachable Cmd on the Jira board: {other:?}"
            );
            Vec::new()
        }
    }
}

/// Run `Cmd::LoadAuditStatus`: build the board's per-ticket audit badge map
/// from live `audit` tmux windows (see [`live_action_tickets`]) and the
/// latest `kind = "audit"` run per ticket, via [`audit_indicator`]'s pure
/// precedence rule.
///
/// `deps.store` being `None` (an unopenable runs DB) yields an empty map
/// rather than an error -- the Jira board must keep working without run-store
/// access. A `tmux.list_windows()` or `store.list_runs_filtered()` failure
/// is likewise treated as "nothing to report" rather than a board-wide error,
/// matching this command's role as best-effort background enrichment, not a
/// primary data load.
fn load_audit_status(deps: &TuiDeps) -> Vec<Msg> {
    let Some(store) = &deps.store else {
        return vec![Msg::AuditStatusLoaded(HashMap::new())];
    };

    let sessions = live_action_tickets(
        &deps.tmux.list_windows().unwrap_or_default(),
        &ticket_session_prefix(&deps.backend_identity.session_slug()),
        AUDIT_WINDOW_NAME,
    );

    // `list_runs_filtered` orders active runs before terminal ones, and by
    // `started_at` descending within each group (see
    // `crate::runs::RunStore::list_runs_filtered`), so the first run seen
    // per ticket here is exactly "the live run if one exists, otherwise the
    // most recent terminal one" -- precisely the run `audit_indicator` wants.
    let mut latest_by_ticket: HashMap<String, (crate::runs::RunStatus, bool)> = HashMap::new();
    for run in store
        .list_runs_filtered(Some(&deps.backend_identity.scope()), Some("audit"))
        .unwrap_or_default()
    {
        latest_by_ticket
            .entry(run.ticket)
            .or_insert((run.status, run.awaiting_input));
    }

    let mut keys: HashSet<&String> = sessions.iter().collect();
    keys.extend(latest_by_ticket.keys());

    let mut status = HashMap::new();
    for key in keys {
        let window_live = sessions.contains(key);
        let run = latest_by_ticket.get(key).copied();
        if let Some(indicator) = audit_indicator(window_live, run) {
            status.insert(
                key.clone(),
                AuditStatusEntry {
                    indicator,
                    window_live,
                },
            );
        }
    }

    vec![Msg::AuditStatusLoaded(status)]
}

/// Run `Cmd::LoadLaneRunStatus`: build the board's per-ticket lane-run badge
/// map from the latest `kind = "lane"` run per ticket, via
/// [`lane_run_indicator`]'s pure precedence rule.
///
/// `deps.store` being `None`, or `list_runs_filtered` failing, yields an
/// empty map rather than an error, mirroring [`load_audit_status`]'s
/// leniency: this is best-effort background enrichment, not a primary data
/// load. Unlike `load_audit_status` there is no tmux-session liveness signal
/// to fold in here -- a lane run's indicator comes purely from its run row
/// (see `docs/plans/board-lane-runs.md`'s "Indicator mapping"), so this
/// always calls [`lane_run_indicator`] with `pending: false`. The
/// pending-launch overlay (`RunIndicator::Starting` for a ticket whose
/// launcher child is still in flight with no run row yet) is applied
/// reducer-side instead, by `Msg::LaneRunStatusLoaded` in `app.rs`, since
/// this function only sees `TuiDeps` and has no access to
/// `App::pending_lane_launches`.
fn load_lane_run_status(deps: &TuiDeps) -> Vec<Msg> {
    let Some(store) = &deps.store else {
        return vec![Msg::LaneRunStatusLoaded(HashMap::new())];
    };

    // `list_runs_filtered` orders active runs before terminal ones, and by
    // `started_at` descending within each group (see
    // `crate::runs::RunStore::list_runs_filtered`), so the first run seen
    // per ticket here is exactly "the live run if one exists, otherwise the
    // most recent terminal one" -- precisely what `lane_run_indicator` wants.
    let mut latest_by_ticket: HashMap<String, (crate::runs::RunStatus, bool)> = HashMap::new();
    for run in store
        .list_runs_filtered(Some(&deps.backend_identity.scope()), Some("lane"))
        .unwrap_or_default()
    {
        latest_by_ticket
            .entry(run.ticket)
            .or_insert((run.status, run.awaiting_input));
    }

    let mut status = HashMap::new();
    for (key, run) in latest_by_ticket {
        if let Some(indicator) = lane_run_indicator(false, Some(run)) {
            status.insert(key, indicator);
        }
    }

    vec![Msg::LaneRunStatusLoaded(status)]
}

/// Run `Cmd::LoadBotWatchStatus`: build the board's per-ticket PR bot-watch
/// badge map from the latest `kind = "review-watch"` run per ticket, via
/// [`bot_watch_indicator`]'s pure mapping.
///
/// Same leniency and same "latest run per ticket" ordering argument as
/// [`load_lane_run_status`], and the same absence of a tmux-session signal:
/// `tm pr watch`'s poll loop is a headless background process, so its run row
/// is the only source of truth (see `docs/plans/bugbot-watch.md`'s "Board
/// integration"). A ticket whose *launcher* child is still in flight has no
/// run row yet; that pending state is rendered from
/// `App::pending_bot_watch_launches` instead, which this function cannot see.
fn load_bot_watch_status(deps: &TuiDeps) -> Vec<Msg> {
    let Some(store) = &deps.store else {
        return vec![Msg::BotWatchStatusLoaded(HashMap::new())];
    };

    let mut latest_by_ticket: HashMap<String, crate::runs::RunStatus> = HashMap::new();
    for run in store
        .list_runs_filtered(
            Some(&deps.backend_identity.scope()),
            Some(REVIEW_WATCH_KIND),
        )
        .unwrap_or_default()
    {
        latest_by_ticket.entry(run.ticket).or_insert(run.status);
    }

    let mut status = HashMap::new();
    for (key, run_status) in latest_by_ticket {
        if let Some(indicator) = bot_watch_indicator(Some(run_status)) {
            status.insert(key, indicator);
        }
    }

    vec![Msg::BotWatchStatusLoaded(status)]
}

/// Run `Cmd::LoadCleanupStatus`: build the board's per-ticket bugbot-cleanup
/// badge map from live `bugbot` tmux windows and the latest
/// `kind = "bugbot-cleanup"` run per ticket.
///
/// Structurally identical to [`load_audit_status`] -- same leniency, same
/// ordering argument, and the *same* [`audit_indicator`] precedence rule and
/// [`AuditStatusEntry`] output type, which were already generic over "a
/// tmux-hosted interactive session kind" rather than audits specifically (see
/// `docs/plans/bugbot-watch.md`'s "Ground truth"). Only the session-name
/// prefix, the window name, and the run `kind` differ.
fn load_cleanup_status(deps: &TuiDeps) -> Vec<Msg> {
    let Some(store) = &deps.store else {
        return vec![Msg::CleanupStatusLoaded(HashMap::new())];
    };

    let sessions = live_action_tickets(
        &deps.tmux.list_windows().unwrap_or_default(),
        &ticket_session_prefix(&deps.backend_identity.session_slug()),
        CLEANUP_WINDOW_NAME,
    );

    let mut latest_by_ticket: HashMap<String, (crate::runs::RunStatus, bool)> = HashMap::new();
    for run in store
        .list_runs_filtered(Some(&deps.backend_identity.scope()), Some(CLEANUP_KIND))
        .unwrap_or_default()
    {
        latest_by_ticket
            .entry(run.ticket)
            .or_insert((run.status, run.awaiting_input));
    }

    let mut keys: HashSet<&String> = sessions.iter().collect();
    keys.extend(latest_by_ticket.keys());

    let mut status = HashMap::new();
    for key in keys {
        let window_live = sessions.contains(key);
        let run = latest_by_ticket.get(key).copied();
        if let Some(indicator) = audit_indicator(window_live, run) {
            status.insert(
                key.clone(),
                AuditStatusEntry {
                    indicator,
                    window_live,
                },
            );
        }
    }

    vec![Msg::CleanupStatusLoaded(status)]
}

/// The ticket keys that currently have a *live window* for action
/// `window_name` in a session named `<session_prefix><lowercased key>` --
/// the board's liveness signal for tmux-hosted actions.
///
/// Window names, not session existence: a ticket's session collects one
/// window per action taken against it, so its existence only says the ticket
/// has been touched (see [`TmuxOps::list_windows`]). A window that exists but
/// whose pane has died (`remain-on-exit`) is aftermath, not a running action,
/// so `dead` windows are excluded.
///
/// Round-trip assumption, unchanged from the session-prefix scheme this
/// replaced: Jira ticket keys are always uppercase (`PROJ-123`) and the
/// session-name functions only ever lowercase -- never otherwise transform --
/// the key, so uppercasing the stripped suffix recovers the original key
/// exactly.
fn live_action_tickets(
    windows: &[crate::work::tmux::TmuxWindow],
    session_prefix: &str,
    window_name: &str,
) -> HashSet<String> {
    windows
        .iter()
        .filter(|window| {
            !window.dead && crate::work::tmux::window_action(&window.name) == window_name
        })
        .filter_map(|window| window.session.strip_prefix(session_prefix))
        .map(str::to_uppercase)
        .collect()
}

/// Session-name prefix every ticket session of the board's own scope shares
/// (see [`crate::work::naming::ticket_session_name`]), and so the prefix
/// [`live_action_tickets`] strips. One prefix for both badge maps now that a
/// ticket's audit and bugbot windows live in the same session -- the *window*
/// name is what tells them apart. Computed from the board's own
/// [`crate::config::BackendIdentity::session_slug`] rather than a bare
/// `tm-`, so sessions belonging to another repo's same-numbered tickets are
/// simply not recognized (GitHub issue #10).
fn ticket_session_prefix(session_slug: &str) -> String {
    format!("tm-{session_slug}-")
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
        &deps.backend_identity,
        key,
    ) {
        Ok(_) if deps.audit_dir_fallback => format!(
            "launched audit for {key} in the current repo (configured audit dir is \
             backend-incompatible) -- press a to attach"
        ),
        Ok(_) => format!("launched audit for {key} -- press a to attach"),
        Err(crate::work::audit::AuditLaunchError::AlreadyRunning {
            session_name,
            window_name,
        }) => {
            format!("audit already running ({session_name}:{window_name}) -- press a to attach")
        }
        Err(err) => err.to_string(),
    };
    vec![Msg::AuditActionResult(message)]
}

/// Run `Cmd::LaunchCreate`: launch the scope's ticket-creation session via
/// [`crate::work::create::launch_create`] and attach to it immediately
/// (issue #15's dictation flow), returning the status-line message the user
/// sees once they detach and land back on the board.
///
/// An [`AlreadyRunning`](crate::work::create::CreateLaunchError::AlreadyRunning)
/// outcome attaches too — a second `c` press is "take me back to my draft",
/// never a duplicate session. Only `NotConfigured` and tmux failures skip
/// the attach, surfacing as a plain status line. Unlike [`launch_audit_cmd`]
/// there is no `deps.store` guard: no run row is pre-registered (the
/// in-session `tm ticket create` registers its own `kind = "create"` run via
/// the session marker), so a broken runs DB never blocks creating tickets.
fn launch_create_and_attach<B: Backend>(terminal: &mut Terminal<B>, deps: &TuiDeps) -> String {
    let session_name = match crate::work::create::launch_create(
        deps.tmux.as_ref(),
        &deps.create,
        &deps.home,
        &deps.backend_identity,
    ) {
        Ok(outcome) => outcome.session_name,
        Err(crate::work::create::CreateLaunchError::AlreadyRunning { session_name, .. }) => {
            session_name
        }
        Err(err) => return err.to_string(),
    };
    let message = attach_session(terminal, deps.tmux.as_ref(), &session_name);
    if deps.create_dir_fallback {
        format!(
            "{message} (create session ran in the current repo; configured create dir is \
             backend-incompatible)"
        )
    } else {
        message
    }
}

/// Run `Cmd::LaunchCleanup`: launch a bugbot-cleanup session for `key` via
/// [`crate::work::bugbot::launch_cleanup`], mapping the outcome to a
/// status-line message. Mirrors [`launch_audit_cmd`] exactly, including its
/// `deps.store` being `None` case: there would be nowhere to pre-register the
/// run row.
fn launch_cleanup_cmd(deps: &TuiDeps, key: &str) -> Vec<Msg> {
    let Some(store) = &deps.store else {
        return vec![Msg::BotsActionResult("runs db unavailable".to_string())];
    };

    let launch_deps = crate::work::bugbot::CleanupLaunchDeps {
        store,
        tmux: deps.tmux.as_ref(),
    };
    let request = crate::work::bugbot::CleanupLaunchRequest {
        cfg: &deps.review_watch,
        home: &deps.home,
        xdg_data_home: deps.xdg_data_home.as_deref(),
        identity: &deps.backend_identity,
        key,
    };

    let message = match crate::work::bugbot::launch_cleanup(&launch_deps, &request) {
        Ok(_) => format!("launched bugbot cleanup for {key} -- press b to attach"),
        Err(crate::work::bugbot::CleanupLaunchError::AlreadyRunning {
            session_name,
            window_name,
        }) => {
            format!(
                "bugbot cleanup already running ({session_name}:{window_name}) -- press b to attach"
            )
        }
        Err(err) => err.to_string(),
    };
    vec![Msg::BotsActionResult(message)]
}

/// Search for tickets matching `query` and map them to
/// [`crate::tui::app::TicketSummary`]s. Shared by `Cmd::FetchTickets` and
/// `Cmd::FetchRankTickets`, which differ only in which `Msg` the result (or
/// error) becomes.
fn search_tickets(deps: &TuiDeps, query: &TicketQuery) -> Result<TicketPage, ProviderError> {
    let result = deps.jira.search(query)?;
    Ok(TicketPage {
        truncated: result.next_page_token.is_some(),
        tickets: result
            .issues
            .into_iter()
            .map(|issue| to_ticket_summary(deps.jira.as_ref(), issue, &deps.base_url))
            .collect(),
    })
}

/// What [`search_tickets`] found: the mapped tickets, plus whether
/// [`crate::ticketing::provider::TicketProvider::search`] stopped on its page budget
/// with more matches unfetched.
struct TicketPage {
    tickets: Vec<TicketSummary>,
    truncated: bool,
}

impl TicketPage {
    /// The truncation warning to append after the screen's loaded message,
    /// or nothing when the results are complete.
    fn truncation_msg(&self) -> Option<Msg> {
        self.truncated.then_some(Msg::SearchTruncated {
            shown: self.tickets.len(),
        })
    }
}

/// Run `Cmd::FetchTickets`: search for tickets matching `query` and map them
/// to [`crate::tui::app::TicketSummary`]s.
fn fetch_tickets(deps: &TuiDeps, query: &TicketQuery) -> Vec<Msg> {
    match search_tickets(deps, query) {
        Ok(page) => {
            let truncation = page.truncation_msg();
            let mut msgs = vec![Msg::TicketsLoaded(page.tickets)];
            msgs.extend(truncation);
            msgs
        }
        Err(err) => vec![Msg::TicketsFailed(err.to_string())],
    }
}

/// Run `Cmd::FetchRankTickets`: search for the project's full ranked ticket
/// list for [`crate::tui::app::Screen::Rank`].
fn fetch_rank_tickets(deps: &TuiDeps, query: &TicketQuery) -> Vec<Msg> {
    match search_tickets(deps, query) {
        Ok(page) => {
            let truncation = page.truncation_msg();
            let mut msgs = vec![Msg::RankTicketsLoaded(page.tickets)];
            msgs.extend(truncation);
            msgs
        }
        Err(err) => vec![Msg::RankTicketsFailed(err.to_string())],
    }
}

/// Run `Cmd::FetchRetroTickets`: search for shipped tickets matching `query`,
/// drop any that already have a recorded retro verdict (via
/// [`crate::runs::RunStore::retro_verdicts_for_tickets`]'s batch lookup), and
/// enrich the rest with their latest `kind = "lane"` run's cost/model info,
/// for [`crate::tui::app::Screen::Retro`].
///
/// Requires `deps.store`: unlike the board's badge polls (which degrade to
/// an empty map when the runs DB is unavailable, since badges are best-effort
/// enrichment of a still-useful board), the retro board's entire reason to
/// exist is filtering by recorded verdicts -- showing an unfiltered list
/// would be actively misleading, not just less enriched. So a missing store
/// fails the whole screen with a status-line message instead.
pub fn fetch_retro_tickets(deps: &TuiDeps, query: &TicketQuery) -> Vec<Msg> {
    // No truncation warning here, unlike the board and rank screens: this
    // list is filtered down again below (tickets with a recorded verdict drop
    // out), so a "showing first N" count taken from the fetch wouldn't match
    // what ends up on screen. The 30-day window keeps it well inside one
    // search's page budget anyway.
    let tickets = match search_tickets(deps, query) {
        Ok(page) => page.tickets,
        Err(err) => return vec![Msg::RetroTicketsFailed(err.to_string())],
    };
    let Some(store) = &deps.store else {
        return vec![Msg::RetroTicketsFailed(
            "run database unavailable".to_string(),
        )];
    };

    let scope = deps.backend_identity.scope();
    let keys: Vec<String> = tickets.iter().map(|t| t.key.clone()).collect();
    let verdicts = match store.retro_verdicts_for_tickets(Some(&scope), &keys) {
        Ok(verdicts) => verdicts,
        Err(err) => return vec![Msg::RetroTicketsFailed(err.to_string())],
    };

    let mut rows = Vec::new();
    for ticket in tickets {
        if verdicts.contains_key(&ticket.key) {
            continue;
        }
        let run = match store.latest_run_for_ticket_kind(Some(&scope), &ticket.key, Some("lane")) {
            Ok(Some(run)) => Some(crate::tui::app::RetroRunInfo {
                cost_usd: run.cost_usd,
                model_summary: run
                    .model_usage
                    .as_deref()
                    .and_then(crate::runs::parse_model_usage)
                    .and_then(|usage| crate::runs::format_model_usage_compact(&usage)),
            }),
            Ok(None) => None,
            Err(err) => return vec![Msg::RetroTicketsFailed(err.to_string())],
        };
        rows.push(crate::tui::app::RetroRow {
            key: ticket.key,
            summary: ticket.summary,
            url: ticket.url,
            run,
        });
    }
    vec![Msg::RetroTicketsLoaded(rows)]
}

/// Run `Cmd::RecordRetro`: record a retro verdict via
/// [`crate::runs::RunStore::record_retro`].
///
/// `deps.store` being unavailable degrades to [`Msg::RetroFailed`] rather
/// than a panic -- the same "never freeze or crash the board" stance every
/// other store-backed command here takes.
pub fn record_retro(
    deps: &TuiDeps,
    key: &str,
    verdict: crate::runs::RetroVerdict,
    severity: Option<crate::runs::RetroSeverity>,
    notes: Option<&str>,
) -> Vec<Msg> {
    let Some(store) = &deps.store else {
        return vec![Msg::RetroFailed("run database unavailable".to_string())];
    };
    match store.record_retro(
        &deps.backend_identity.scope(),
        key,
        verdict,
        severity,
        notes,
    ) {
        Ok(()) => vec![Msg::RetroRecorded {
            key: key.to_string(),
            verdict,
        }],
        Err(err) => vec![Msg::RetroFailed(err.to_string())],
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

/// Run `Cmd::AssignTicket`: resolve `choice` to a target account/display name
/// and call [`crate::ticketing::provider::TicketProvider::assign`].
///
/// [`AssignChoice::Me`] goes through [`crate::ticketing::provider::TicketProvider::myself`]
/// rather than any cached account ID: the only cache available to the board
/// TUI is a Jira accountId
/// ([`crate::config::Config::default_assignee_account_id`]), which is
/// meaningless under the GitHub
/// backend (its assignee is a login, not a Jira accountId). `myself()` is
/// also the only source of a human-readable display name for the card, which
/// a raw accountId/login lookup wouldn't give us for free.
fn assign_ticket_cmd(deps: &TuiDeps, key: &str, choice: &AssignChoice) -> Vec<Msg> {
    let (account_id, display_name) = match choice {
        AssignChoice::Me => match deps.jira.myself() {
            Ok(myself) => (Some(myself.account_id), Some(myself.display_name)),
            Err(err) => return vec![Msg::AssignFailed(format!("assign {key} failed: {err}"))],
        },
        AssignChoice::Unassign => (None, None),
        AssignChoice::User(user) => (
            Some(user.account_id.clone()),
            Some(user.display_name.clone()),
        ),
    };

    match deps.jira.assign(key, account_id.as_deref()) {
        Ok(()) => vec![Msg::AssignApplied {
            key: key.to_string(),
            assignee: display_name,
        }],
        Err(err) => vec![Msg::AssignFailed(format!("assign {key} failed: {err}"))],
    }
}

/// Run `Cmd::ResolvePrForTicket`: look up `key`'s open GitHub PR (if any),
/// one `gh pr list` call, and report the result as
/// [`Msg::BrowserOptionsResolved`]. Never fails outright -- every failure
/// mode (no runs DB, `git`/`gh` erroring, no PR resolving to `key`) degrades
/// to `pr: None`, letting [`crate::tui::app::browser_options_resolved`] open
/// Jira directly rather than leaving the user stuck, per [`TuiDeps`]'s
/// leniency stance.
///
/// This is the one place `tui` calls a network-backed command synchronously
/// on a keypress rather than deferring it to a background poll (contrast
/// [`load_bot_watch_status`] et al., which only ever read the local runs DB).
/// That's accepted here, not just tolerated, for the same reason
/// [`TuiDeps::store`]'s doc comment and [`load_audit_status`]'s
/// `.unwrap_or_default()` rationale accept their own leniency: a *bounded*
/// block is a reasonable price for a feature that only fires on an explicit
/// keypress (never prefetched for every card), and [`run_cmds`] makes sure
/// the status line reflects that wait rather than leaving the board looking
/// hung. The bound itself -- [`crate::github::gh_cli::GhCli::pr_list_bounded`]
/// with [`PR_LOOKUP_TIMEOUT`] -- is what makes "reasonable" actually true:
/// before it existed, `gh pr list` had no timeout at all, so a dead network
/// or expired `gh` auth froze the *entire* board (no redraw, no key input,
/// not even quit) for as long as the hang lasted, which is the defect this
/// whole mechanism exists to close. On timeout specifically, `note` carries a
/// status-line explanation (rather than silently landing on Jira looking
/// like "no PR found") -- see [`crate::tui::app::browser_options_resolved`].
fn resolve_pr_for_ticket(deps: &TuiDeps, key: String, jira_url: String) -> Vec<Msg> {
    let Some(repo_root) = resolve_repo_root_for_pr_lookup(deps, &key) else {
        return vec![Msg::BrowserOptionsResolved {
            key,
            jira_url,
            pr: None,
            note: None,
        }];
    };

    match deps.gh.pr_list_bounded(&repo_root, PR_LOOKUP_TIMEOUT) {
        Ok(prs) => {
            let pr = crate::github::pr::find_pr_for_ticket(&prs, &key).cloned();
            vec![Msg::BrowserOptionsResolved {
                key,
                jira_url,
                pr,
                note: None,
            }]
        }
        Err(crate::github::gh_cli::GhError::Timeout { .. }) => {
            let note = format!(
                "PR lookup for {key} timed out after {}s; opening Jira",
                PR_LOOKUP_TIMEOUT.as_secs()
            );
            vec![Msg::BrowserOptionsResolved {
                key,
                jira_url,
                pr: None,
                note: Some(note),
            }]
        }
        Err(_) => vec![Msg::BrowserOptionsResolved {
            key,
            jira_url,
            pr: None,
            note: None,
        }],
    }
}

/// Resolve the repository `key`'s `gh pr list` lookup should run against, for
/// [`resolve_pr_for_ticket`].
///
/// Reuses [`crate::cli::pr::resolve_watch_repo_root`] -- the same "ticket's
/// lane repo, falling back to `cwd`'s git repo root" resolution `tm pr watch`
/// already relies on to answer this exact question (see that function's doc
/// comment) -- whenever a runs DB is open. `None` on any resolution failure
/// (a `RunStoreError` or `GitError`), which [`resolve_pr_for_ticket`] folds
/// into "no PR found" rather than an error.
///
/// When `deps.store` is `None` (an unopenable runs DB), there is no way to
/// look up the ticket's lane run, so this skips straight to
/// `resolve_watch_repo_root`'s own fallback -- `deps.git.repo_root(deps.cwd)`
/// -- directly. That fallback is only correct when `deps.cwd` really is the
/// ticket's repo (the same caveat `resolve_watch_repo_root`'s doc comment
/// already accepts for the lane-less/no-store case), which holds for the
/// ordinary case of running `tm board` from inside the repo.
fn resolve_repo_root_for_pr_lookup(deps: &TuiDeps, key: &str) -> Option<std::path::PathBuf> {
    match &deps.store {
        Some(store) => crate::cli::pr::resolve_watch_repo_root(
            Some(&deps.backend_identity.scope()),
            &deps.lanes,
            store,
            deps.git.as_ref(),
            &deps.cwd,
            key,
        )
        .ok(),
        None => deps.git.repo_root(&deps.cwd).ok(),
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
/// deriving `url` from `base_url` and `description` from
/// [`TicketProvider::description_text`].
fn to_ticket_summary(
    jira: &dyn TicketProvider,
    issue: Issue,
    base_url: &str,
) -> crate::tui::app::TicketSummary {
    let description = jira.description_text(&issue);
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
    use crate::ticketing::provider::JiraProvider;
    use crate::ticketing::types::{IssueFields, JiraUser, Myself, Status, StatusCategory};

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
    /// (which requires `&mut Terminal` for [`Cmd::AttachSession`]'s
    /// suspend/restore, even though most tests never exercise that path).
    fn test_terminal() -> Terminal<ratatui::backend::TestBackend> {
        Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal should build")
    }

    fn deps(jira: FakeJiraClient) -> TuiDeps {
        TuiDeps {
            jira: Box::new(JiraProvider::new(jira)),
            base_url: "https://example.atlassian.net".to_string(),
            project_key: "PROJ".to_string(),
            board_column_order: Vec::new(),
            store: None,
            tmux: Box::new(crate::work::tmux::FakeTmuxOps::new()),
            audit: crate::config::AuditConfig::default(),
            create: crate::config::CreateConfig::default(),
            home: std::path::PathBuf::from("/home/test"),
            review_watch: crate::config::ReviewWatchConfig::default(),
            xdg_data_home: None,
            launcher: Box::new(crate::tui::launcher::FakeLaneLauncher::new()),
            lane_names: Vec::new(),
            hidden_lane_count: 0,
            audit_dir_fallback: false,
            create_dir_fallback: false,
            gh: Box::new(crate::github::gh_cli::FakeGhCli::new()),
            git: Box::new(crate::work::git::FakeGitOps::new()),
            cwd: std::path::PathBuf::from("/repo"),
            lanes: std::collections::BTreeMap::new(),
            backend_identity: crate::config::BackendIdentity::Jira {
                base_url: "https://x.atlassian.net".to_string(),
                project_key: "PROJ".to_string(),
            },
        }
    }

    #[test]
    fn fetch_tickets_maps_issues_to_ticket_summaries() {
        use crate::ticketing::types::SearchResult;

        let jira = FakeJiraClient::new().with_search_result(SearchResult {
            issues: vec![issue("PROJ-1", "To Do")],
            next_page_token: None,
        });
        let msgs = fetch_tickets(&deps(jira), &TicketQuery::MyOpen);
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
        let msgs = fetch_tickets(&deps(jira), &TicketQuery::MyOpen);
        assert_eq!(msgs, vec![Msg::TicketsLoaded(vec![])]);
    }

    #[test]
    fn fetch_tickets_appends_search_truncated_when_a_page_was_left_unfollowed() {
        use crate::ticketing::types::SearchResult;

        let jira = FakeJiraClient::new().with_search_result(SearchResult {
            issues: vec![issue("PROJ-1", "To Do"), issue("PROJ-2", "To Do")],
            next_page_token: Some("more".to_string()),
        });
        let msgs = fetch_tickets(&deps(jira), &TicketQuery::MyOpen);
        match msgs.as_slice() {
            [Msg::TicketsLoaded(tickets), Msg::SearchTruncated { shown }] => {
                assert_eq!(tickets.len(), 2);
                assert_eq!(*shown, 2);
            }
            other => panic!("expected TicketsLoaded then SearchTruncated, got {other:?}"),
        }
    }

    #[test]
    fn fetch_tickets_failure_emits_tickets_failed() {
        let jira = FakeJiraClient::new().with_search_error(500, "boom");
        let msgs = fetch_tickets(&deps(jira), &TicketQuery::MyOpen);
        match msgs.as_slice() {
            [Msg::TicketsFailed(message)] => {
                assert_eq!(message, "ticket provider API error (500): boom")
            }
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
                assert_eq!(message, "ticket provider API error (500): boom")
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
    fn assign_ticket_to_user_emits_assign_applied_with_display_name() {
        let jira = FakeJiraClient::new();
        let choice = AssignChoice::User(JiraUser {
            account_id: "acct-1".to_string(),
            display_name: "Jane Doe".to_string(),
        });
        let msgs = assign_ticket_cmd(&deps(jira), "PROJ-1", &choice);
        assert_eq!(
            msgs,
            vec![Msg::AssignApplied {
                key: "PROJ-1".to_string(),
                assignee: Some("Jane Doe".to_string()),
            }]
        );
    }

    #[test]
    fn assign_ticket_unassign_emits_assign_applied_with_none() {
        let jira = FakeJiraClient::new();
        let d = deps(jira);
        let msgs = assign_ticket_cmd(&d, "PROJ-1", &AssignChoice::Unassign);
        assert_eq!(
            msgs,
            vec![Msg::AssignApplied {
                key: "PROJ-1".to_string(),
                assignee: None,
            }]
        );
    }

    #[test]
    fn assign_ticket_me_resolves_myself_then_assigns_with_resolved_id() {
        let jira = FakeJiraClient::new().with_myself(Myself {
            account_id: "acct-1".to_string(),
            display_name: "Ada Lovelace".to_string(),
            email_address: None,
        });
        let msgs = assign_ticket_cmd(&deps(jira), "PROJ-1", &AssignChoice::Me);
        assert_eq!(
            msgs,
            vec![Msg::AssignApplied {
                key: "PROJ-1".to_string(),
                assignee: Some("Ada Lovelace".to_string()),
            }]
        );
    }

    #[test]
    fn assign_ticket_me_myself_failure_emits_assign_failed_without_assigning() {
        let jira = FakeJiraClient::new().with_myself_unauthorized();
        let msgs = assign_ticket_cmd(&deps(jira), "PROJ-1", &AssignChoice::Me);
        match msgs.as_slice() {
            [Msg::AssignFailed(message)] => {
                assert!(message.starts_with("assign PROJ-1 failed:"));
            }
            other => panic!("expected AssignFailed, got {other:?}"),
        }
    }

    #[test]
    fn assign_ticket_assign_failure_emits_assign_failed() {
        let jira = FakeJiraClient::new().with_assign_error(500, "boom");
        let choice = AssignChoice::User(JiraUser {
            account_id: "acct-1".to_string(),
            display_name: "Jane Doe".to_string(),
        });
        let msgs = assign_ticket_cmd(&deps(jira), "PROJ-1", &choice);
        match msgs.as_slice() {
            [Msg::AssignFailed(message)] => {
                assert!(message.starts_with("assign PROJ-1 failed:"));
            }
            other => panic!("expected AssignFailed, got {other:?}"),
        }
    }

    fn pr(number: u64, title: &str, branch: &str) -> crate::github::pr::PrInfo {
        crate::github::pr::PrInfo {
            number,
            url: format!("https://github.com/example/repo/pull/{number}"),
            title: title.to_string(),
            body: String::new(),
            head_ref_name: branch.to_string(),
        }
    }

    #[test]
    fn resolve_pr_for_ticket_finds_pr_via_git_fallback_when_no_runs_store() {
        let mut deps = deps(FakeJiraClient::new());
        deps.git = Box::new(
            crate::work::git::FakeGitOps::new()
                .with_repo_root(Ok(std::path::PathBuf::from("/repo"))),
        );
        deps.gh = Box::new(
            crate::github::gh_cli::FakeGhCli::new().with_pr_list(Ok(vec![pr(
                42,
                "[PROJ-1] Fix the thing",
                "proj-1-fix",
            )])),
        );
        let msgs = resolve_pr_for_ticket(&deps, "PROJ-1".to_string(), "jira-url".to_string());
        assert_eq!(
            msgs,
            vec![Msg::BrowserOptionsResolved {
                key: "PROJ-1".to_string(),
                jira_url: "jira-url".to_string(),
                pr: Some(pr(42, "[PROJ-1] Fix the thing", "proj-1-fix")),
                note: None,
            }]
        );
    }

    #[test]
    fn resolve_pr_for_ticket_with_no_matching_pr_resolves_none() {
        let mut deps = deps(FakeJiraClient::new());
        deps.git = Box::new(
            crate::work::git::FakeGitOps::new()
                .with_repo_root(Ok(std::path::PathBuf::from("/repo"))),
        );
        deps.gh = Box::new(
            crate::github::gh_cli::FakeGhCli::new().with_pr_list(Ok(vec![pr(
                7,
                "unrelated PR",
                "some-branch",
            )])),
        );
        let msgs = resolve_pr_for_ticket(&deps, "PROJ-1".to_string(), "jira-url".to_string());
        assert_eq!(
            msgs,
            vec![Msg::BrowserOptionsResolved {
                key: "PROJ-1".to_string(),
                jira_url: "jira-url".to_string(),
                pr: None,
                note: None,
            }]
        );
    }

    #[test]
    fn resolve_pr_for_ticket_degrades_to_none_when_repo_root_fails() {
        let mut deps = deps(FakeJiraClient::new());
        deps.git = Box::new(crate::work::git::FakeGitOps::new().with_repo_root(Err(
            crate::work::git::GitError::Command {
                command: "git rev-parse".to_string(),
                exit_code: Some(128),
                stderr: "not a git repository".to_string(),
            },
        )));
        let msgs = resolve_pr_for_ticket(&deps, "PROJ-1".to_string(), "jira-url".to_string());
        assert_eq!(
            msgs,
            vec![Msg::BrowserOptionsResolved {
                key: "PROJ-1".to_string(),
                jira_url: "jira-url".to_string(),
                pr: None,
                note: None,
            }]
        );
    }

    #[test]
    fn resolve_pr_for_ticket_degrades_to_none_when_gh_pr_list_fails() {
        let mut deps = deps(FakeJiraClient::new());
        deps.git = Box::new(
            crate::work::git::FakeGitOps::new()
                .with_repo_root(Ok(std::path::PathBuf::from("/repo"))),
        );
        deps.gh = Box::new(crate::github::gh_cli::FakeGhCli::new().with_pr_list(Err(
            crate::github::gh_cli::GhError::Spawn {
                command: "gh pr list".to_string(),
                message: "gh not found".to_string(),
            },
        )));
        let msgs = resolve_pr_for_ticket(&deps, "PROJ-1".to_string(), "jira-url".to_string());
        assert_eq!(
            msgs,
            vec![Msg::BrowserOptionsResolved {
                key: "PROJ-1".to_string(),
                jira_url: "jira-url".to_string(),
                pr: None,
                note: None,
            }]
        );
    }

    #[test]
    fn resolve_pr_for_ticket_on_timeout_falls_back_to_jira_with_a_status_note() {
        let mut deps = deps(FakeJiraClient::new());
        deps.git = Box::new(
            crate::work::git::FakeGitOps::new()
                .with_repo_root(Ok(std::path::PathBuf::from("/repo"))),
        );
        deps.gh = Box::new(
            crate::github::gh_cli::FakeGhCli::new().with_pr_list_bounded(Err(
                crate::github::gh_cli::GhError::Timeout {
                    command: "gh pr list".to_string(),
                    seconds: PR_LOOKUP_TIMEOUT.as_secs(),
                },
            )),
        );
        let msgs = resolve_pr_for_ticket(&deps, "PROJ-1".to_string(), "jira-url".to_string());
        match msgs.as_slice() {
            [
                Msg::BrowserOptionsResolved {
                    key,
                    jira_url,
                    pr,
                    note: Some(note),
                },
            ] => {
                assert_eq!(key, "PROJ-1");
                assert_eq!(jira_url, "jira-url");
                assert_eq!(*pr, None);
                assert_eq!(
                    note,
                    "PR lookup for PROJ-1 timed out after 8s; opening Jira"
                );
            }
            other => panic!("expected BrowserOptionsResolved with a timeout note, got {other:?}"),
        }
    }

    #[test]
    fn to_ticket_summary_derives_url_and_extracts_description() {
        let summary = to_ticket_summary(
            &FakeJiraClient::new(),
            issue("PROJ-1", "To Do"),
            "https://example.atlassian.net",
        );
        assert_eq!(summary.key, "PROJ-1");
        assert_eq!(summary.status, "To Do");
        assert_eq!(summary.url, "https://example.atlassian.net/browse/PROJ-1");
        assert_eq!(summary.description, "Body text");
    }

    #[test]
    fn to_ticket_summary_with_no_description_is_empty_string() {
        let mut issue = issue("PROJ-1", "To Do");
        issue.fields.description = None;
        let summary = to_ticket_summary(
            &FakeJiraClient::new(),
            issue,
            "https://example.atlassian.net",
        );
        assert_eq!(summary.description, "");
    }

    #[test]
    fn to_ticket_summary_with_no_assignee_is_none() {
        let summary = to_ticket_summary(
            &FakeJiraClient::new(),
            issue("PROJ-1", "To Do"),
            "https://example.atlassian.net",
        );
        assert_eq!(summary.assignee, None);
    }

    #[test]
    fn to_ticket_summary_with_assignee_extracts_display_name() {
        use crate::ticketing::types::UserRef;

        let mut issue = issue("PROJ-1", "To Do");
        issue.fields.assignee = Some(UserRef {
            account_id: "acct-1".to_string(),
            display_name: "Jane Doe".to_string(),
        });
        let summary = to_ticket_summary(
            &FakeJiraClient::new(),
            issue,
            "https://example.atlassian.net",
        );
        assert_eq!(summary.assignee, Some("Jane Doe".to_string()));
    }

    #[test]
    fn fetch_rank_tickets_maps_issues_to_ticket_summaries() {
        use crate::ticketing::types::SearchResult;

        let jira = FakeJiraClient::new().with_search_result(SearchResult {
            issues: vec![issue("PROJ-1", "To Do")],
            next_page_token: None,
        });
        let msgs = fetch_rank_tickets(
            &deps(jira),
            &TicketQuery::Ranked {
                project_key: "PROJ".to_string(),
            },
        );
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
        let jira = FakeJiraClient::new().with_search_error(500, "boom");
        let msgs = fetch_rank_tickets(
            &deps(jira),
            &TicketQuery::Ranked {
                project_key: "PROJ".to_string(),
            },
        );
        match msgs.as_slice() {
            [Msg::RankTicketsFailed(message)] => {
                assert_eq!(message, "ticket provider API error (500): boom")
            }
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
            [Msg::RankFailed(message)] => {
                assert_eq!(message, "ticket provider API error (500): boom")
            }
            other => panic!("expected RankFailed, got {other:?}"),
        }
    }

    // --- Cmd::FetchRetroTickets / fetch_retro_tickets ---

    #[test]
    fn fetch_retro_tickets_with_no_store_reports_unavailable() {
        use crate::ticketing::types::SearchResult;

        let jira = FakeJiraClient::new().with_search_result(SearchResult {
            issues: vec![issue("PROJ-1", "Done")],
            next_page_token: None,
        });
        let mut deps = deps(jira);
        deps.store = None;
        let msgs = fetch_retro_tickets(
            &deps,
            &TicketQuery::ShippedAwaitingRetro {
                project_key: "PROJ".to_string(),
            },
        );
        assert_eq!(
            msgs,
            vec![Msg::RetroTicketsFailed(
                "run database unavailable".to_string()
            )]
        );
    }

    #[test]
    fn fetch_retro_tickets_search_failure_reports_failed() {
        let jira = FakeJiraClient::new().with_search_error(500, "boom");
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();
        let mut deps = deps(jira);
        deps.store = Some(store);
        let msgs = fetch_retro_tickets(
            &deps,
            &TicketQuery::ShippedAwaitingRetro {
                project_key: "PROJ".to_string(),
            },
        );
        match msgs.as_slice() {
            [Msg::RetroTicketsFailed(message)] => {
                assert_eq!(message, "ticket provider API error (500): boom")
            }
            other => panic!("expected RetroTicketsFailed, got {other:?}"),
        }
    }

    #[test]
    fn fetch_retro_tickets_excludes_tickets_with_a_recorded_verdict() {
        use crate::ticketing::types::SearchResult;

        let jira = FakeJiraClient::new().with_search_result(SearchResult {
            issues: vec![issue("PROJ-1", "Done"), issue("PROJ-2", "Done")],
            next_page_token: None,
        });
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();
        store
            .record_retro("", "PROJ-1", crate::runs::RetroVerdict::Clean, None, None)
            .unwrap();
        let mut deps = deps(jira);
        deps.store = Some(store);

        let msgs = fetch_retro_tickets(
            &deps,
            &TicketQuery::ShippedAwaitingRetro {
                project_key: "PROJ".to_string(),
            },
        );
        match msgs.as_slice() {
            [Msg::RetroTicketsLoaded(rows)] => {
                let keys: Vec<&str> = rows.iter().map(|r| r.key.as_str()).collect();
                assert_eq!(keys, vec!["PROJ-2"]);
            }
            other => panic!("expected RetroTicketsLoaded, got {other:?}"),
        }
    }

    #[test]
    fn fetch_retro_tickets_ticket_with_no_run_has_run_none() {
        use crate::ticketing::types::SearchResult;

        let jira = FakeJiraClient::new().with_search_result(SearchResult {
            issues: vec![issue("PROJ-1", "Done")],
            next_page_token: None,
        });
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();
        let mut deps = deps(jira);
        deps.store = Some(store);

        let msgs = fetch_retro_tickets(
            &deps,
            &TicketQuery::ShippedAwaitingRetro {
                project_key: "PROJ".to_string(),
            },
        );
        match msgs.as_slice() {
            [Msg::RetroTicketsLoaded(rows)] => {
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].key, "PROJ-1");
                assert_eq!(rows[0].run, None);
            }
            other => panic!("expected RetroTicketsLoaded, got {other:?}"),
        }
    }

    #[test]
    fn fetch_retro_tickets_ticket_with_a_lane_run_reports_cost_and_model_mix() {
        use crate::ticketing::types::SearchResult;

        let jira = FakeJiraClient::new().with_search_result(SearchResult {
            issues: vec![issue("PROJ-1", "Done")],
            next_page_token: None,
        });
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();
        let run_id = store.start_run(&lane_start_params("PROJ-1")).unwrap();
        store
            .finish_run(
                run_id,
                &crate::runs::FinishRun {
                    status: crate::runs::RunStatus::Done,
                    cost_usd: Some(4.5),
                    model_usage: Some(
                        r#"{"claude-fable-5":{"outputTokens":58564,"costUSD":4.5}}"#.to_string(),
                    ),
                    ..crate::runs::FinishRun::default()
                },
            )
            .unwrap();
        let mut deps = deps(jira);
        deps.store = Some(store);

        let msgs = fetch_retro_tickets(
            &deps,
            &TicketQuery::ShippedAwaitingRetro {
                project_key: "PROJ".to_string(),
            },
        );
        match msgs.as_slice() {
            [Msg::RetroTicketsLoaded(rows)] => {
                let run = rows[0].run.as_ref().expect("expected a run");
                assert_eq!(run.cost_usd, Some(4.5));
                assert_eq!(run.model_summary.as_deref(), Some("fable-5 58.6k out"));
            }
            other => panic!("expected RetroTicketsLoaded, got {other:?}"),
        }
    }

    // --- Cmd::RecordRetro / record_retro ---

    #[test]
    fn record_retro_clean_success_emits_retro_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();
        let mut deps = deps(FakeJiraClient::new());
        deps.store = Some(store);

        let msgs = record_retro(
            &deps,
            "PROJ-1",
            crate::runs::RetroVerdict::Clean,
            None,
            None,
        );
        assert_eq!(
            msgs,
            vec![Msg::RetroRecorded {
                key: "PROJ-1".to_string(),
                verdict: crate::runs::RetroVerdict::Clean,
            }]
        );
    }

    #[test]
    fn record_retro_defect_with_severity_and_note_success() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();
        let mut deps = deps(FakeJiraClient::new());
        deps.store = Some(store);

        let msgs = record_retro(
            &deps,
            "PROJ-1",
            crate::runs::RetroVerdict::Defect,
            Some(crate::runs::RetroSeverity::Critical),
            Some("it broke prod"),
        );
        assert_eq!(
            msgs,
            vec![Msg::RetroRecorded {
                key: "PROJ-1".to_string(),
                verdict: crate::runs::RetroVerdict::Defect,
            }]
        );
        let recorded = deps
            .store
            .as_ref()
            .unwrap()
            .latest_retro_for_ticket(None, "PROJ-1")
            .unwrap()
            .expect("expected a recorded retro");
        assert_eq!(
            recorded.severity,
            Some(crate::runs::RetroSeverity::Critical)
        );
        assert_eq!(recorded.notes.as_deref(), Some("it broke prod"));
    }

    #[test]
    fn record_retro_defect_with_no_severity_reports_the_store_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();
        let mut deps = deps(FakeJiraClient::new());
        deps.store = Some(store);

        let msgs = record_retro(
            &deps,
            "PROJ-1",
            crate::runs::RetroVerdict::Defect,
            None,
            None,
        );
        match msgs.as_slice() {
            [Msg::RetroFailed(message)] => {
                assert_eq!(
                    message,
                    "--severity is required when recording a defect retro"
                );
            }
            other => panic!("expected RetroFailed, got {other:?}"),
        }
    }

    #[test]
    fn record_retro_with_no_store_reports_unavailable() {
        let mut deps = deps(FakeJiraClient::new());
        deps.store = None;
        let msgs = record_retro(
            &deps,
            "PROJ-1",
            crate::runs::RetroVerdict::Clean,
            None,
            None,
        );
        assert_eq!(
            msgs,
            vec![Msg::RetroFailed("run database unavailable".to_string())]
        );
    }

    #[test]
    fn run_cmds_feeds_tickets_loaded_back_through_update() {
        use crate::ticketing::types::SearchResult;

        let jira = FakeJiraClient::new().with_search_result(SearchResult {
            issues: vec![issue("PROJ-1", "To Do")],
            next_page_token: None,
        });
        let mut terminal = test_terminal();
        let mut launches = Vec::new();
        let app = run_cmds(
            App::new(),
            vec![Cmd::FetchTickets {
                query: TicketQuery::MyOpen,
            }],
            &deps(jira),
            &mut terminal,
            &mut launches,
        );
        assert_eq!(app.columns.len(), 1);
        assert_eq!(app.selected_col, 0);
        assert_eq!(app.selected_row, 0);
    }

    /// Flatten a [`ratatui::backend::TestBackend`]'s buffer into one string,
    /// for asserting a message is visible on screen. Mirrors `src/tui/ui.rs`'s
    /// own private `buffer_text` test helper.
    fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
        let mut out = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn run_cmds_draws_the_resolving_status_line_before_the_blocking_pr_lookup() {
        // `open_browser_action` (src/tui/app.rs) is what actually sets this
        // status line on a real `o` keypress; this test starts from its
        // output directly to isolate what's under test here: that
        // `run_cmds` paints that message to the terminal *before* running
        // `Cmd::ResolvePrForTicket`'s blocking `gh` call, rather than only on
        // the event loop's next iteration (which -- per this fix's whole
        // premise -- would happen only *after* the call already finished).
        let app = App {
            status_line: "resolving PR for PROJ-1...".to_string(),
            ..App::new()
        };
        let mut terminal = test_terminal();
        let mut launches = Vec::new();
        let mut deps = deps(FakeJiraClient::new());
        deps.git = Box::new(
            crate::work::git::FakeGitOps::new()
                .with_repo_root(Ok(std::path::PathBuf::from("/repo"))),
        );
        // Resolve to a matching PR (rather than the "no PR" default) so
        // `browser_options_resolved` shows the picker and emits no further
        // `Cmd` -- specifically not `Cmd::OpenUrl`, which would shell out to
        // the real `open` command and could actually launch a browser during
        // this test.
        deps.gh = Box::new(
            crate::github::gh_cli::FakeGhCli::new().with_pr_list(Ok(vec![pr(
                42,
                "[PROJ-1] Fix the thing",
                "proj-1-fix",
            )])),
        );

        run_cmds(
            app,
            vec![Cmd::ResolvePrForTicket {
                key: "PROJ-1".to_string(),
                jira_url: "jira-url".to_string(),
            }],
            &deps,
            &mut terminal,
            &mut launches,
        );

        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("resolving PR for PROJ-1..."),
            "expected the resolving status line to have been drawn before the \
             blocking lookup ran, got:\n{text}"
        );
    }

    fn watch_deps(store: crate::runs::RunStore) -> WatchDeps {
        WatchDeps { store }
    }

    fn start_params(ticket: &str) -> crate::runs::StartRun {
        crate::runs::StartRun {
            scope: String::new(),
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

    // --- live_action_tickets ---

    fn window(session: &str, name: &str, dead: bool) -> crate::work::tmux::TmuxWindow {
        crate::work::tmux::TmuxWindow {
            session: session.to_string(),
            name: name.to_string(),
            dead,
        }
    }

    #[test]
    fn live_action_tickets_maps_the_owning_session_back_to_its_ticket_key() {
        let windows = vec![window("tm-proj-proj-1", "audit", false)];
        assert_eq!(
            live_action_tickets(&windows, &ticket_session_prefix("proj"), AUDIT_WINDOW_NAME),
            HashSet::from(["PROJ-1".to_string()])
        );
    }

    #[test]
    fn live_action_tickets_ignores_dead_windows() {
        // A window whose pane exited (`remain-on-exit`) is aftermath, not a
        // running action.
        let windows = vec![window("tm-proj-proj-1", "audit", true)];
        assert!(
            live_action_tickets(&windows, &ticket_session_prefix("proj"), AUDIT_WINDOW_NAME)
                .is_empty()
        );
    }

    #[test]
    fn live_action_tickets_ignores_other_windows_in_the_same_session() {
        // The whole point of window-name liveness: a ticket's session can be
        // up with only unrelated windows in it.
        let windows = vec![
            window("tm-proj-proj-1", "shell", false),
            window("tm-proj-proj-1", "fix", false),
        ];
        assert!(
            live_action_tickets(&windows, &ticket_session_prefix("proj"), AUDIT_WINDOW_NAME)
                .is_empty()
        );
    }

    #[test]
    fn live_action_tickets_ignores_sessions_without_the_prefix() {
        let windows = vec![window("axiom-lane", "audit", false)];
        assert!(
            live_action_tickets(&windows, &ticket_session_prefix("proj"), AUDIT_WINDOW_NAME)
                .is_empty()
        );
    }

    #[test]
    fn live_action_tickets_does_not_cross_match_the_other_action_kind() {
        let windows = vec![
            window("tm-proj-proj-1", AUDIT_WINDOW_NAME, false),
            window("tm-proj-proj-2", CLEANUP_WINDOW_NAME, false),
        ];
        assert_eq!(
            live_action_tickets(&windows, &ticket_session_prefix("proj"), AUDIT_WINDOW_NAME),
            HashSet::from(["PROJ-1".to_string()])
        );
        assert_eq!(
            live_action_tickets(
                &windows,
                &ticket_session_prefix("proj"),
                CLEANUP_WINDOW_NAME
            ),
            HashSet::from(["PROJ-2".to_string()])
        );
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
            crate::work::tmux::FakeTmuxOps::new().with_list_windows(Ok(vec![
                crate::work::tmux::TmuxWindow {
                    session: "tm-proj-proj-1".to_string(),
                    name: "audit".to_string(),
                    dead: false,
                },
            ])),
        );

        let msgs = load_audit_status(&deps);
        match msgs.as_slice() {
            [Msg::AuditStatusLoaded(status)] => {
                let entry = status.get("PROJ-1").expect("PROJ-1 should have an entry");
                assert_eq!(entry.indicator, crate::tui::app::AuditIndicator::Running);
                assert!(entry.window_live);
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
            crate::work::tmux::FakeTmuxOps::new().with_list_windows(Ok(vec![
                crate::work::tmux::TmuxWindow {
                    session: "tm-proj-proj-2".to_string(),
                    name: "audit".to_string(),
                    dead: false,
                },
            ])),
        );

        let msgs = load_audit_status(&deps);
        match msgs.as_slice() {
            [Msg::AuditStatusLoaded(status)] => {
                let entry = status.get("PROJ-2").expect("PROJ-2 should have an entry");
                assert_eq!(entry.indicator, crate::tui::app::AuditIndicator::Starting);
                assert!(entry.window_live);
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
        // No live `audit` window: `FakeTmuxOps::new()`'s default
        // `list_windows` is `Ok(vec![])`.

        let msgs = load_audit_status(&deps);
        assert_eq!(msgs, vec![Msg::AuditStatusLoaded(HashMap::new())]);
    }

    // --- Cmd::LoadTicketRunDetail / load_ticket_run_detail ---

    #[test]
    fn load_ticket_run_detail_with_no_store_reports_unavailable() {
        let mut deps = deps(FakeJiraClient::new());
        deps.store = None;
        let msgs = load_ticket_run_detail(&deps, "PROJ-1");
        assert_eq!(
            msgs,
            vec![Msg::RunDetailFailed("run database unavailable".to_string())]
        );
    }

    #[test]
    fn load_ticket_run_detail_with_no_runs_reports_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();

        let mut deps = deps(FakeJiraClient::new());
        deps.store = Some(store);

        let msgs = load_ticket_run_detail(&deps, "PROJ-1");
        assert_eq!(
            msgs,
            vec![Msg::RunDetailFailed("no runs for PROJ-1".to_string())]
        );
    }

    #[test]
    fn load_ticket_run_detail_picks_the_latest_of_several_runs() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();
        store.start_run(&start_params("PROJ-1")).unwrap();
        let latest_id = store.start_run(&start_params("PROJ-1")).unwrap();

        let mut deps = deps(FakeJiraClient::new());
        deps.store = Some(store);

        let msgs = load_ticket_run_detail(&deps, "PROJ-1");
        match msgs.as_slice() {
            [Msg::RunDetailLoaded(detail)] => assert_eq!(detail.id, latest_id),
            other => panic!("expected RunDetailLoaded, got {other:?}"),
        }
    }

    #[test]
    fn load_ticket_run_detail_is_kind_agnostic() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();
        store.start_run(&audit_start_params("PROJ-1")).unwrap();

        let mut deps = deps(FakeJiraClient::new());
        deps.store = Some(store);

        let msgs = load_ticket_run_detail(&deps, "PROJ-1");
        match msgs.as_slice() {
            [Msg::RunDetailLoaded(detail)] => assert_eq!(detail.kind, "audit"),
            other => panic!("expected RunDetailLoaded, got {other:?}"),
        }
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
            model: None,
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
    fn launch_audit_cmd_success_with_fallback_notes_backend_mismatch() {
        // GitHub issue #5 phase 2: `deps.audit_dir_fallback` is set by
        // `main.rs` when the configured audit dir was already redirected to
        // the current repo for a backend mismatch; the launch message
        // should say so rather than the plain "launched audit" text.
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();

        let mut deps = deps(FakeJiraClient::new());
        deps.store = Some(store);
        deps.audit = crate::config::AuditConfig {
            dir: Some("/repo/tskmstr".to_string()),
            prompt: None,
            model: None,
        };
        deps.audit_dir_fallback = true;

        let msgs = launch_audit_cmd(&deps, "PROJ-1");
        match msgs.as_slice() {
            [Msg::AuditActionResult(message)] => {
                assert!(
                    message.contains("backend-incompatible"),
                    "expected a backend-mismatch note, got: {message}"
                );
            }
            other => panic!("expected AuditActionResult, got {other:?}"),
        }
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
            model: None,
        };
        deps.tmux = Box::new(
            crate::work::tmux::FakeTmuxOps::new().with_list_windows(Ok(vec![window(
                "tm-proj-proj-1",
                AUDIT_WINDOW_NAME,
                false,
            )])),
        );

        let msgs = launch_audit_cmd(&deps, "PROJ-1");
        assert_eq!(
            msgs,
            vec![Msg::AuditActionResult(
                "audit already running (tm-proj-proj-1:audit) -- press a to attach".to_string()
            )]
        );
    }

    // --- Cmd::AttachSession routing (run_cmds intercepts it before `execute`) ---

    #[test]
    fn run_cmds_intercepts_audit_attach_and_reports_status_line() {
        let d = deps(FakeJiraClient::new());
        let mut terminal = test_terminal();
        let mut launches = Vec::new();
        let app = run_cmds(
            App::new(),
            vec![Cmd::AttachSession {
                session_name: "tm-proj-proj-1".to_string(),
            }],
            &d,
            &mut terminal,
            &mut launches,
        );
        assert_eq!(app.status_line, "detached from tm-proj-proj-1");
    }

    /// Phase 5's board attach reuses `attach_session`'s suspend/restore dance
    /// exactly, so it routes through `run_cmds` the same way and reports the
    /// same shape of status line.
    #[test]
    fn run_cmds_intercepts_attach_session_and_reports_status_line() {
        let d = deps(FakeJiraClient::new());
        let mut terminal = test_terminal();
        let mut launches = Vec::new();
        let app = run_cmds(
            App::new(),
            vec![Cmd::AttachSession {
                session_name: "tm-proj-proj-1".to_string(),
            }],
            &d,
            &mut terminal,
            &mut launches,
        );
        assert_eq!(app.status_line, "detached from tm-proj-proj-1");
    }

    /// From inside tmux, attach runs `switch-client` and returns immediately
    /// while the user is now looking at the target session -- the status
    /// line must not claim they detached (issue #6).
    #[test]
    fn run_cmds_attach_reports_switch_wording_when_client_switched() {
        let mut d = deps(FakeJiraClient::new());
        d.tmux = Box::new(
            crate::work::tmux::FakeTmuxOps::new()
                .with_attach_outcome(crate::work::tmux::AttachOutcome::Switched),
        );
        let mut terminal = test_terminal();
        let mut launches = Vec::new();
        let app = run_cmds(
            App::new(),
            vec![Cmd::AttachSession {
                session_name: "tm-proj-proj-1".to_string(),
            }],
            &d,
            &mut terminal,
            &mut launches,
        );
        assert_eq!(app.status_line, "switched client to tm-proj-proj-1");
    }

    // --- Cmd::LaunchCreate routing (run_cmds intercepts it before `execute`,
    //     issue #15) ---

    /// A `deps` whose `[work.create]` is configured and whose tmux fake
    /// reports `windows` (each `(session, window, dead)`).
    fn create_deps(windows: &[(&str, &str, bool)]) -> TuiDeps {
        let mut d = deps(FakeJiraClient::new());
        d.create = crate::config::CreateConfig {
            dir: Some("/repo/axiom".to_string()),
            prompt: None,
            model: None,
        };
        d.tmux = Box::new(
            crate::work::tmux::FakeTmuxOps::new().with_list_windows(Ok(windows
                .iter()
                .map(|(session, name, dead)| crate::work::tmux::TmuxWindow {
                    session: (*session).to_string(),
                    name: (*name).to_string(),
                    dead: *dead,
                })
                .collect())),
        );
        d
    }

    #[test]
    fn run_cmds_launch_create_unconfigured_reports_status_line_without_attaching() {
        // `deps()` leaves `create.dir` unset; the status line must be the
        // NotConfigured message, not an attach outcome.
        let d = deps(FakeJiraClient::new());
        let mut terminal = test_terminal();
        let mut launches = Vec::new();
        let app = run_cmds(
            App::new(),
            vec![Cmd::LaunchCreate],
            &d,
            &mut terminal,
            &mut launches,
        );
        assert_eq!(
            app.status_line,
            "ticket creation is not configured; set [work.create].dir"
        );
    }

    #[test]
    fn run_cmds_launch_create_launches_then_attaches_immediately() {
        // The launch/attach tmux call sequence itself is pinned by
        // `crate::work::create`'s unit tests; this covers the routing: one
        // `Cmd` ends in `attach_session`'s message, with no second keypress.
        let d = create_deps(&[]);
        let mut terminal = test_terminal();
        let mut launches = Vec::new();
        let app = run_cmds(
            App::new(),
            vec![Cmd::LaunchCreate],
            &d,
            &mut terminal,
            &mut launches,
        );
        assert_eq!(app.status_line, "detached from tm-proj-create");
    }

    #[test]
    fn run_cmds_launch_create_attaches_to_a_live_create_session_instead_of_erroring() {
        // With the create window already live, `launch_create` reports
        // AlreadyRunning -- which must read as "attach to the draft", never
        // surface as an error message (issue #15's deliberate re-entry).
        let d = create_deps(&[("tm-proj-create", "create", false)]);
        let mut terminal = test_terminal();
        let mut launches = Vec::new();
        let app = run_cmds(
            App::new(),
            vec![Cmd::LaunchCreate],
            &d,
            &mut terminal,
            &mut launches,
        );
        assert_eq!(app.status_line, "detached from tm-proj-create");
    }

    #[test]
    fn run_cmds_launch_create_notes_the_dir_fallback_in_the_status_line() {
        let mut d = create_deps(&[]);
        d.create_dir_fallback = true;
        let mut terminal = test_terminal();
        let mut launches = Vec::new();
        let app = run_cmds(
            App::new(),
            vec![Cmd::LaunchCreate],
            &d,
            &mut terminal,
            &mut launches,
        );
        assert_eq!(
            app.status_line,
            "detached from tm-proj-create (create session ran in the current repo; configured \
             create dir is backend-incompatible)"
        );
    }

    // --- Cmd::LaunchLaneRun routing (run_cmds intercepts it before `execute`) ---

    #[test]
    fn run_cmds_launch_lane_run_success_registers_pending_launch() {
        let d = deps(FakeJiraClient::new());
        let mut terminal = test_terminal();
        let mut launches = Vec::new();
        let app = run_cmds(
            App::new(),
            vec![Cmd::LaunchLaneRun {
                lane: "backend".to_string(),
                key: "PROJ-1".to_string(),
            }],
            &d,
            &mut terminal,
            &mut launches,
        );
        assert_eq!(launches.len(), 1);
        assert_eq!(launches[0].key, "PROJ-1");
        // No result has been fed through `update` yet -- the launcher's
        // fake handle hasn't been polled -- so `app` is untouched.
        assert_eq!(app.status_line, "");
    }

    #[test]
    fn run_cmds_launch_lane_run_spawn_failure_feeds_immediate_launch_result() {
        let mut d = deps(FakeJiraClient::new());
        d.launcher = Box::new(
            crate::tui::launcher::FakeLaneLauncher::new().with_spawn_error("no such lane"),
        );
        let mut terminal = test_terminal();
        let mut launches = Vec::new();
        let app = run_cmds(
            App::new(),
            vec![Cmd::LaunchLaneRun {
                lane: "bogus".to_string(),
                key: "PROJ-1".to_string(),
            }],
            &d,
            &mut terminal,
            &mut launches,
        );
        assert!(launches.is_empty());
        assert_eq!(
            app.status_line,
            "lane run launch failed for PROJ-1: no such lane"
        );
    }

    /// A [`LaneLauncher`] that records the argv it was handed into a shared
    /// buffer the test can inspect after [`run_cmds`] has consumed
    /// [`TuiDeps`]'s boxed launcher. [`crate::tui::launcher::FakeLaneLauncher`]
    /// records the same thing but is moved into the box, so its `calls()` is
    /// no longer reachable from the test.
    struct RecordingLauncher(std::rc::Rc<std::cell::RefCell<Vec<Vec<String>>>>);

    impl crate::tui::launcher::LaneLauncher for RecordingLauncher {
        fn spawn(
            &self,
            argv: &[String],
        ) -> Result<Box<dyn crate::tui::launcher::LaunchHandle>, String> {
            self.0.borrow_mut().push(argv.to_vec());
            crate::tui::launcher::FakeLaneLauncher::new()
                .with_finish_sequence(vec![None])
                .spawn(argv)
        }
    }

    #[test]
    fn run_cmds_launch_lane_run_spawns_work_run_argv() {
        let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut d = deps(FakeJiraClient::new());
        d.launcher = Box::new(RecordingLauncher(calls.clone()));
        let mut terminal = test_terminal();
        let mut launches = Vec::new();
        run_cmds(
            App::new(),
            vec![Cmd::LaunchLaneRun {
                lane: "backend".to_string(),
                key: "PROJ-1".to_string(),
            }],
            &d,
            &mut terminal,
            &mut launches,
        );
        assert_eq!(
            calls.borrow().clone(),
            vec![vec![
                "work".to_string(),
                "run".to_string(),
                "backend".to_string(),
                "PROJ-1".to_string(),
            ]]
        );
    }

    // --- Cmd::LaunchBotWatch routing (run_cmds intercepts it too) ---

    #[test]
    fn run_cmds_launch_bot_watch_spawns_pr_watch_argv() {
        let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut d = deps(FakeJiraClient::new());
        d.launcher = Box::new(RecordingLauncher(calls.clone()));
        let mut terminal = test_terminal();
        let mut launches = Vec::new();
        run_cmds(
            App::new(),
            vec![Cmd::LaunchBotWatch {
                key: "PROJ-1".to_string(),
            }],
            &d,
            &mut terminal,
            &mut launches,
        );
        assert_eq!(
            calls.borrow().clone(),
            vec![vec![
                "pr".to_string(),
                "watch".to_string(),
                "PROJ-1".to_string(),
            ]]
        );
        assert_eq!(launches.len(), 1);
    }

    #[test]
    fn run_cmds_launch_bot_watch_spawn_failure_feeds_immediate_launch_result() {
        let mut d = deps(FakeJiraClient::new());
        d.launcher =
            Box::new(crate::tui::launcher::FakeLaneLauncher::new().with_spawn_error("boom"));
        let mut terminal = test_terminal();
        let mut launches = Vec::new();
        let app = run_cmds(
            App::new(),
            vec![Cmd::LaunchBotWatch {
                key: "PROJ-1".to_string(),
            }],
            &d,
            &mut terminal,
            &mut launches,
        );
        assert!(launches.is_empty());
        assert_eq!(app.status_line, "PR watch failed for PROJ-1: boom");
    }

    #[test]
    fn poll_pending_launches_reports_a_bot_watch_entry_as_bot_watch_result() {
        let launcher =
            crate::tui::launcher::FakeLaneLauncher::new().with_finish_sequence(vec![Some(Ok(()))]);
        let handle = launcher.spawn(&bot_watch_argv("PROJ-1")).unwrap();
        let mut launches = vec![PendingLaunch {
            key: "PROJ-1".to_string(),
            kind: PendingLaunchKind::BotWatch,
            handle,
        }];
        let msgs = poll_pending_launches(&mut launches);
        assert_eq!(
            msgs,
            vec![Msg::BotWatchLaunchResult {
                key: "PROJ-1".to_string(),
                result: Ok(()),
            }]
        );
        assert!(launches.is_empty());
    }

    // --- Cmd::LaunchReviewFix routing (run_cmds intercepts it too) ---

    #[test]
    fn run_cmds_launch_review_fix_spawns_review_fix_argv() {
        let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut d = deps(FakeJiraClient::new());
        d.launcher = Box::new(RecordingLauncher(calls.clone()));
        let mut terminal = test_terminal();
        let mut launches = Vec::new();
        run_cmds(
            App::new(),
            vec![Cmd::LaunchReviewFix {
                key: "PROJ-1".to_string(),
            }],
            &d,
            &mut terminal,
            &mut launches,
        );
        assert_eq!(
            calls.borrow().clone(),
            vec![vec![
                "review".to_string(),
                "fix".to_string(),
                "PROJ-1".to_string(),
            ]]
        );
        assert_eq!(launches.len(), 1);
    }

    #[test]
    fn run_cmds_launch_review_fix_success_registers_pending_launch() {
        let d = deps(FakeJiraClient::new());
        let mut terminal = test_terminal();
        let mut launches = Vec::new();
        let app = run_cmds(
            App::new(),
            vec![Cmd::LaunchReviewFix {
                key: "PROJ-1".to_string(),
            }],
            &d,
            &mut terminal,
            &mut launches,
        );
        assert_eq!(launches.len(), 1);
        assert_eq!(launches[0].key, "PROJ-1");
        assert_eq!(app.status_line, "");
    }

    #[test]
    fn run_cmds_launch_review_fix_spawn_failure_feeds_immediate_launch_result() {
        let mut d = deps(FakeJiraClient::new());
        d.launcher = Box::new(
            crate::tui::launcher::FakeLaneLauncher::new().with_spawn_error("current_exe failed"),
        );
        let mut terminal = test_terminal();
        let mut launches = Vec::new();
        let app = run_cmds(
            App::new(),
            vec![Cmd::LaunchReviewFix {
                key: "PROJ-1".to_string(),
            }],
            &d,
            &mut terminal,
            &mut launches,
        );
        assert!(launches.is_empty());
        assert_eq!(
            app.status_line,
            "fix pass for PROJ-1 failed: current_exe failed"
        );
    }

    #[test]
    fn poll_pending_launches_reports_a_review_fix_entry_as_review_fix_result() {
        let launcher = crate::tui::launcher::FakeLaneLauncher::new()
            .with_finish_sequence(vec![Some(Err("no comments captured".to_string()))]);
        let handle = launcher.spawn(&review_fix_argv("PROJ-1")).unwrap();
        let mut launches = vec![PendingLaunch {
            key: "PROJ-1".to_string(),
            kind: PendingLaunchKind::ReviewFix,
            handle,
        }];
        let msgs = poll_pending_launches(&mut launches);
        assert_eq!(
            msgs,
            vec![Msg::ReviewFixLaunchResult {
                key: "PROJ-1".to_string(),
                result: Err("no comments captured".to_string()),
            }]
        );
        assert!(launches.is_empty());
    }

    // --- resolve_vdiff_worktree (Cmd::ViewDiff's testable resolution logic) ---

    #[test]
    fn vdiff_launches_the_terminal_frontend() {
        assert!(
            VDIFF_ARGS.contains(&"--tui"),
            "the board runs inside a terminal; vdiff must open its TUI, not the GUI"
        );
    }

    /// A flag glued onto the program name (`Command::new("vdiff --tui")`)
    /// makes the whole string the executable to look up, so the launch fails
    /// with `NotFound` and the board reports "vdiff not found on PATH" no
    /// matter how correctly vdiff is installed.
    #[test]
    fn vdiff_program_is_a_bare_executable_name() {
        assert!(
            !VDIFF_PROGRAM.contains(char::is_whitespace),
            "flags belong in VDIFF_ARGS, not the program name: {VDIFF_PROGRAM:?}"
        );
    }

    #[test]
    fn resolve_vdiff_worktree_with_no_store_reports_unavailable() {
        let result = resolve_vdiff_worktree(None, None, "PROJ-1");
        assert_eq!(result, Err("no run store available".to_string()));
    }

    #[test]
    fn resolve_vdiff_worktree_with_no_lane_run_reports_no_run() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();
        let result = resolve_vdiff_worktree(Some(&store), None, "PROJ-1");
        assert_eq!(result, Err("no lane run for PROJ-1".to_string()));
    }

    #[test]
    fn resolve_vdiff_worktree_ignores_a_non_lane_run() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();
        let worktree = tempfile::tempdir().unwrap();
        store
            .start_run(&crate::runs::StartRun {
                scope: String::new(),
                ticket: "PROJ-1".to_string(),
                lane: "backend".to_string(),
                worktree: worktree.path().to_string_lossy().to_string(),
                branch: None,
                pid: None,
                kind: "audit".to_string(),
                log_path: None,
            })
            .unwrap();
        let result = resolve_vdiff_worktree(Some(&store), None, "PROJ-1");
        assert_eq!(result, Err("no lane run for PROJ-1".to_string()));
    }

    #[test]
    fn resolve_vdiff_worktree_with_a_missing_worktree_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();
        let gone = dir.path().join("gone-worktree");
        store
            .start_run(&crate::runs::StartRun {
                scope: String::new(),
                ticket: "PROJ-1".to_string(),
                lane: "backend".to_string(),
                worktree: gone.to_string_lossy().to_string(),
                branch: None,
                pid: None,
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();
        let result = resolve_vdiff_worktree(Some(&store), None, "PROJ-1");
        assert_eq!(
            result,
            Err(format!(
                "worktree {} for PROJ-1 no longer exists",
                gone.display()
            ))
        );
    }

    #[test]
    fn resolve_vdiff_worktree_with_an_existing_worktree_resolves_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();
        let worktree = tempfile::tempdir().unwrap();
        store
            .start_run(&crate::runs::StartRun {
                scope: String::new(),
                ticket: "PROJ-1".to_string(),
                lane: "backend".to_string(),
                worktree: worktree.path().to_string_lossy().to_string(),
                branch: None,
                pid: None,
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();
        let result = resolve_vdiff_worktree(Some(&store), None, "PROJ-1");
        assert_eq!(result, Ok(worktree.path().to_path_buf()));
    }

    // --- Cmd::LaunchCleanup / launch_cleanup_cmd ---

    #[test]
    fn launch_cleanup_cmd_with_no_store_reports_unavailable() {
        let mut deps = deps(FakeJiraClient::new());
        deps.store = None;
        let msgs = launch_cleanup_cmd(&deps, "PROJ-1");
        assert_eq!(
            msgs,
            vec![Msg::BotsActionResult("runs db unavailable".to_string())]
        );
    }

    #[test]
    fn launch_cleanup_cmd_success_reports_launched_message() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();

        let mut deps = deps(FakeJiraClient::new());
        deps.store = Some(store);
        deps.review_watch = crate::config::ReviewWatchConfig {
            dir: Some("/repo/axiom".to_string()),
            ..crate::config::ReviewWatchConfig::default()
        };

        let msgs = launch_cleanup_cmd(&deps, "PROJ-1");
        assert_eq!(
            msgs,
            vec![Msg::BotsActionResult(
                "launched bugbot cleanup for PROJ-1 -- press b to attach".to_string()
            )]
        );
    }

    #[test]
    fn launch_cleanup_cmd_not_configured_surfaces_error_text() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();

        let mut deps = deps(FakeJiraClient::new());
        deps.store = Some(store);
        // `deps.review_watch` defaults to no `dir`.

        let msgs = launch_cleanup_cmd(&deps, "PROJ-1");
        match msgs.as_slice() {
            [Msg::BotsActionResult(message)] => {
                assert!(message.contains("[work.review_watch].dir"));
            }
            other => panic!("expected BotsActionResult, got {other:?}"),
        }
    }

    #[test]
    fn launch_cleanup_cmd_already_running_reports_attach_hint() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();

        let mut deps = deps(FakeJiraClient::new());
        deps.store = Some(store);
        deps.review_watch = crate::config::ReviewWatchConfig {
            dir: Some("/repo/axiom".to_string()),
            ..crate::config::ReviewWatchConfig::default()
        };
        deps.tmux = Box::new(
            crate::work::tmux::FakeTmuxOps::new().with_list_windows(Ok(vec![window(
                "tm-proj-proj-1",
                CLEANUP_WINDOW_NAME,
                false,
            )])),
        );

        let msgs = launch_cleanup_cmd(&deps, "PROJ-1");
        assert_eq!(
            msgs,
            vec![Msg::BotsActionResult(
                "bugbot cleanup already running (tm-proj-proj-1:bugbot) -- press b to attach"
                    .to_string()
            )]
        );
    }

    // --- poll_pending_launches ---

    #[test]
    fn poll_pending_launches_leaves_still_running_entries_in_place() {
        let launcher =
            crate::tui::launcher::FakeLaneLauncher::new().with_finish_sequence(vec![None]);
        let handle = launcher.spawn(&lane_run_argv("backend", "PROJ-1")).unwrap();
        let mut launches = vec![PendingLaunch {
            key: "PROJ-1".to_string(),
            kind: PendingLaunchKind::LaneRun,
            handle,
        }];
        let msgs = poll_pending_launches(&mut launches);
        assert!(msgs.is_empty());
        assert_eq!(launches.len(), 1);
    }

    #[test]
    fn poll_pending_launches_removes_and_reports_completed_entries() {
        let launcher =
            crate::tui::launcher::FakeLaneLauncher::new().with_finish_sequence(vec![Some(Ok(()))]);
        let handle = launcher.spawn(&lane_run_argv("backend", "PROJ-1")).unwrap();
        let mut launches = vec![PendingLaunch {
            key: "PROJ-1".to_string(),
            kind: PendingLaunchKind::LaneRun,
            handle,
        }];
        let msgs = poll_pending_launches(&mut launches);
        assert_eq!(
            msgs,
            vec![Msg::LaneRunLaunchResult {
                key: "PROJ-1".to_string(),
                result: Ok(()),
            }]
        );
        assert!(launches.is_empty());
    }

    #[test]
    fn poll_pending_launches_reports_nonzero_exit_as_err() {
        let launcher = crate::tui::launcher::FakeLaneLauncher::new()
            .with_finish_sequence(vec![Some(Err("boom".to_string()))]);
        let handle = launcher.spawn(&lane_run_argv("backend", "PROJ-1")).unwrap();
        let mut launches = vec![PendingLaunch {
            key: "PROJ-1".to_string(),
            kind: PendingLaunchKind::LaneRun,
            handle,
        }];
        let msgs = poll_pending_launches(&mut launches);
        assert_eq!(
            msgs,
            vec![Msg::LaneRunLaunchResult {
                key: "PROJ-1".to_string(),
                result: Err("boom".to_string()),
            }]
        );
        assert!(launches.is_empty());
    }

    // --- Cmd::LoadLaneRunStatus / load_lane_run_status ---

    fn lane_start_params(ticket: &str) -> crate::runs::StartRun {
        crate::runs::StartRun {
            kind: "lane".to_string(),
            ..start_params(ticket)
        }
    }

    #[test]
    fn load_lane_run_status_with_no_store_yields_empty_map() {
        let mut deps = deps(FakeJiraClient::new());
        deps.store = None;
        let msgs = load_lane_run_status(&deps);
        assert_eq!(msgs, vec![Msg::LaneRunStatusLoaded(HashMap::new())]);
    }

    #[test]
    fn load_lane_run_status_running_maps_to_running() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();
        store.start_run(&lane_start_params("PROJ-1")).unwrap();

        let mut deps = deps(FakeJiraClient::new());
        deps.store = Some(store);

        let msgs = load_lane_run_status(&deps);
        match msgs.as_slice() {
            [Msg::LaneRunStatusLoaded(status)] => {
                assert_eq!(
                    status.get("PROJ-1"),
                    Some(&crate::tui::app::RunIndicator::Running)
                );
            }
            other => panic!("expected LaneRunStatusLoaded, got {other:?}"),
        }
    }

    #[test]
    fn load_lane_run_status_running_and_awaiting_maps_to_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();
        let run_id = store.start_run(&lane_start_params("PROJ-1")).unwrap();
        store.add_event(run_id, "await", None).unwrap();

        let mut deps = deps(FakeJiraClient::new());
        deps.store = Some(store);

        let msgs = load_lane_run_status(&deps);
        match msgs.as_slice() {
            [Msg::LaneRunStatusLoaded(status)] => {
                assert_eq!(
                    status.get("PROJ-1"),
                    Some(&crate::tui::app::RunIndicator::Waiting)
                );
            }
            other => panic!("expected LaneRunStatusLoaded, got {other:?}"),
        }
    }

    #[test]
    fn load_lane_run_status_blocked_maps_to_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();
        let run_id = store.start_run(&lane_start_params("PROJ-1")).unwrap();
        store
            .finish_run(
                run_id,
                &crate::runs::FinishRun {
                    status: crate::runs::RunStatus::Blocked,
                    ..crate::runs::FinishRun::default()
                },
            )
            .unwrap();

        let mut deps = deps(FakeJiraClient::new());
        deps.store = Some(store);

        let msgs = load_lane_run_status(&deps);
        match msgs.as_slice() {
            [Msg::LaneRunStatusLoaded(status)] => {
                assert_eq!(
                    status.get("PROJ-1"),
                    Some(&crate::tui::app::RunIndicator::Waiting)
                );
            }
            other => panic!("expected LaneRunStatusLoaded, got {other:?}"),
        }
    }

    #[test]
    fn load_lane_run_status_done_maps_to_done() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();
        let run_id = store.start_run(&lane_start_params("PROJ-1")).unwrap();
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

        let msgs = load_lane_run_status(&deps);
        match msgs.as_slice() {
            [Msg::LaneRunStatusLoaded(status)] => {
                assert_eq!(
                    status.get("PROJ-1"),
                    Some(&crate::tui::app::RunIndicator::Done)
                );
            }
            other => panic!("expected LaneRunStatusLoaded, got {other:?}"),
        }
    }

    #[test]
    fn load_lane_run_status_failed_maps_to_failed() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();
        let run_id = store.start_run(&lane_start_params("PROJ-1")).unwrap();
        store
            .finish_run(
                run_id,
                &crate::runs::FinishRun {
                    status: crate::runs::RunStatus::Failed,
                    ..crate::runs::FinishRun::default()
                },
            )
            .unwrap();

        let mut deps = deps(FakeJiraClient::new());
        deps.store = Some(store);

        let msgs = load_lane_run_status(&deps);
        match msgs.as_slice() {
            [Msg::LaneRunStatusLoaded(status)] => {
                assert_eq!(
                    status.get("PROJ-1"),
                    Some(&crate::tui::app::RunIndicator::Failed)
                );
            }
            other => panic!("expected LaneRunStatusLoaded, got {other:?}"),
        }
    }

    // --- Cmd::LoadBotWatchStatus / load_bot_watch_status ---

    fn review_watch_start_params(ticket: &str) -> crate::runs::StartRun {
        crate::runs::StartRun {
            kind: "review-watch".to_string(),
            lane: "review-watch".to_string(),
            ..start_params(ticket)
        }
    }

    fn cleanup_start_params(ticket: &str) -> crate::runs::StartRun {
        crate::runs::StartRun {
            kind: "bugbot-cleanup".to_string(),
            lane: "bugbot-cleanup".to_string(),
            ..start_params(ticket)
        }
    }

    /// Finish `run_id` with `status`, the shorthand every badge-status test
    /// here needs to reach a terminal run row.
    fn finish(store: &crate::runs::RunStore, run_id: i64, status: crate::runs::RunStatus) {
        store
            .finish_run(
                run_id,
                &crate::runs::FinishRun {
                    status,
                    ..crate::runs::FinishRun::default()
                },
            )
            .unwrap();
    }

    #[test]
    fn load_bot_watch_status_with_no_store_yields_empty_map() {
        let mut deps = deps(FakeJiraClient::new());
        deps.store = None;
        let msgs = load_bot_watch_status(&deps);
        assert_eq!(msgs, vec![Msg::BotWatchStatusLoaded(HashMap::new())]);
    }

    #[test]
    fn load_bot_watch_status_running_maps_to_watching() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();
        store
            .start_run(&review_watch_start_params("PROJ-1"))
            .unwrap();

        let mut deps = deps(FakeJiraClient::new());
        deps.store = Some(store);

        let msgs = load_bot_watch_status(&deps);
        match msgs.as_slice() {
            [Msg::BotWatchStatusLoaded(status)] => {
                assert_eq!(
                    status.get("PROJ-1"),
                    Some(&crate::tui::app::BotWatchIndicator::Watching)
                );
            }
            other => panic!("expected BotWatchStatusLoaded, got {other:?}"),
        }
    }

    #[test]
    fn load_bot_watch_status_review_maps_to_ready() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();
        let run_id = store
            .start_run(&review_watch_start_params("PROJ-1"))
            .unwrap();
        finish(&store, run_id, crate::runs::RunStatus::Review);

        let mut deps = deps(FakeJiraClient::new());
        deps.store = Some(store);

        let msgs = load_bot_watch_status(&deps);
        match msgs.as_slice() {
            [Msg::BotWatchStatusLoaded(status)] => {
                assert_eq!(
                    status.get("PROJ-1"),
                    Some(&crate::tui::app::BotWatchIndicator::Ready)
                );
            }
            other => panic!("expected BotWatchStatusLoaded, got {other:?}"),
        }
    }

    #[test]
    fn load_bot_watch_status_excludes_other_kinds() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();
        store.start_run(&lane_start_params("PROJ-1")).unwrap();
        store.start_run(&cleanup_start_params("PROJ-2")).unwrap();

        let mut deps = deps(FakeJiraClient::new());
        deps.store = Some(store);

        let msgs = load_bot_watch_status(&deps);
        assert_eq!(msgs, vec![Msg::BotWatchStatusLoaded(HashMap::new())]);
    }

    // --- Cmd::LoadCleanupStatus / load_cleanup_status ---

    #[test]
    fn load_cleanup_status_with_no_store_yields_empty_map() {
        let mut deps = deps(FakeJiraClient::new());
        deps.store = None;
        let msgs = load_cleanup_status(&deps);
        assert_eq!(msgs, vec![Msg::CleanupStatusLoaded(HashMap::new())]);
    }

    #[test]
    fn load_cleanup_status_marks_running_cleanup_with_matching_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();
        store.start_run(&cleanup_start_params("PROJ-1")).unwrap();

        let mut deps = deps(FakeJiraClient::new());
        deps.store = Some(store);
        deps.tmux = Box::new(
            crate::work::tmux::FakeTmuxOps::new().with_list_windows(Ok(vec![
                crate::work::tmux::TmuxWindow {
                    session: "tm-proj-proj-1".to_string(),
                    name: CLEANUP_WINDOW_NAME.to_string(),
                    dead: false,
                },
            ])),
        );

        let msgs = load_cleanup_status(&deps);
        match msgs.as_slice() {
            [Msg::CleanupStatusLoaded(status)] => {
                let entry = status.get("PROJ-1").expect("PROJ-1 should have an entry");
                assert_eq!(entry.indicator, crate::tui::app::AuditIndicator::Running);
                assert!(entry.window_live);
            }
            other => panic!("expected CleanupStatusLoaded, got {other:?}"),
        }
    }

    #[test]
    fn load_cleanup_status_marks_starting_when_session_exists_with_no_run() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();

        let mut deps = deps(FakeJiraClient::new());
        deps.store = Some(store);
        deps.tmux = Box::new(
            crate::work::tmux::FakeTmuxOps::new().with_list_windows(Ok(vec![
                crate::work::tmux::TmuxWindow {
                    session: "tm-proj-proj-2".to_string(),
                    name: CLEANUP_WINDOW_NAME.to_string(),
                    dead: false,
                },
            ])),
        );

        let msgs = load_cleanup_status(&deps);
        match msgs.as_slice() {
            [Msg::CleanupStatusLoaded(status)] => {
                let entry = status.get("PROJ-2").expect("PROJ-2 should have an entry");
                assert_eq!(entry.indicator, crate::tui::app::AuditIndicator::Starting);
                assert!(entry.window_live);
            }
            other => panic!("expected CleanupStatusLoaded, got {other:?}"),
        }
    }

    #[test]
    fn load_cleanup_status_ignores_audit_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();

        let mut deps = deps(FakeJiraClient::new());
        deps.store = Some(store);
        deps.tmux = Box::new(
            crate::work::tmux::FakeTmuxOps::new().with_list_windows(Ok(vec![
                crate::work::tmux::TmuxWindow {
                    session: "tm-proj-proj-1".to_string(),
                    name: "audit".to_string(),
                    dead: false,
                },
            ])),
        );

        let msgs = load_cleanup_status(&deps);
        assert_eq!(msgs, vec![Msg::CleanupStatusLoaded(HashMap::new())]);
    }

    #[test]
    fn load_cleanup_status_omits_finished_run_with_no_live_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();
        let run_id = store.start_run(&cleanup_start_params("PROJ-3")).unwrap();
        finish(&store, run_id, crate::runs::RunStatus::Done);

        let mut deps = deps(FakeJiraClient::new());
        deps.store = Some(store);

        let msgs = load_cleanup_status(&deps);
        assert_eq!(msgs, vec![Msg::CleanupStatusLoaded(HashMap::new())]);
    }

    #[test]
    fn load_lane_run_status_excludes_audit_kind_runs() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runs::RunStore::open(&dir.path().join("runs.db")).unwrap();
        store.start_run(&audit_start_params("PROJ-1")).unwrap();

        let mut deps = deps(FakeJiraClient::new());
        deps.store = Some(store);

        let msgs = load_lane_run_status(&deps);
        assert_eq!(msgs, vec![Msg::LaneRunStatusLoaded(HashMap::new())]);
    }
}
