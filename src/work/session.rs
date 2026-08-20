//! Ticket-session reconstruction: issue #2 phase 4 of
//! `docs/plans/one-session-per-ticket.md`.
//!
//! A ticket's `tm-<key>` session is convenience, not state. It dies with the
//! tmux server — a reboot, a `tmux kill-server`, an accidental `tmux
//! kill-session -t tm-proj-1` — while the run rows and log files that record
//! what actually happened do not. [`plan_session`] rebuilds the session from
//! those rows, so losing it costs nothing but a keystroke.
//!
//! # What gets a window, and what does not
//!
//! Only runs that are **still in flight**. This is the load-bearing decision
//! of the whole command, so the reasoning is worth stating:
//!
//! - A *headless* run in flight has a supervisor that survived whatever
//!   killed tmux (that is exactly what [`crate::work::detach`]'s `setsid` is
//!   for) and a log file it is still appending to. Its viewer window
//!   reattaches and the run keeps going, none the wiser.
//! - An *interactive* run in flight has lost its `claude` process: it lived
//!   in the tmux pane. Nothing can bring that conversation back into the
//!   window it was in. What survives is its session id, so the window comes
//!   back as a **plain shell rooted in the run's worktree** and the command
//!   prints the `claude --resume` line for it. Resuming is deliberately *not*
//!   automatic: it starts billing and starts editing, and if the run's
//!   process somehow did survive (a second tmux server, a detached client),
//!   resuming would drive the same session twice.
//! - A *finished* run gets **no window at all**, interactive or headless.
//!   Reconstruction restores working state, not history. A ticket with five
//!   finished runs would otherwise come back as five dead panes whose only
//!   content is a log file that `tm runs logs` (and the board's `L`) already
//!   open on demand. For a finished *interactive* run there is not even a log
//!   — its durable artifact is its prompt file — so a window for it would be
//!   an empty shell claiming to be an action. The run table is the history;
//!   tmux is where live things are.
//!
//! # Idempotence
//!
//! Every planned window is checked against the live window list first, so
//! running this against a healthy session is a no-op, and running it against
//! a half-killed one adds back exactly what is missing. That check uses
//! [`crate::work::tmux::has_live_window`] — the same action-name matching the
//! double-launch guard uses — so a live `fix-2` counts as the ticket's `fix`
//! window already being present.

use crate::runs::{Run, RunStatus};
use crate::work::audit::{AUDIT_WINDOW_NAME, SHELL_WINDOW_NAME};
use crate::work::bugbot::CLEANUP_WINDOW_NAME;
use crate::work::interactive::{FIX_WINDOW_NAME, WORK_WINDOW_NAME};
use crate::work::naming::ticket_session_name;
use crate::work::tmux::{
    TmuxError, TmuxOps, has_live_window, session_window_names, unique_window_name,
};

/// The window name an action of run `kind` takes in a ticket's session.
///
/// The four kinds with a dedicated action window are mapped explicitly; the
/// window is named for the *action*, not the run `kind`, which is why `lane`
/// becomes `work` and `bugbot-cleanup` becomes `bugbot` (see phase 2 of the
/// plan doc). Any other kind falls back to the kind itself rather than being
/// dropped: a new run kind should show up in a reconstructed session as soon
/// as it exists, under a name that is at worst unlovely, instead of silently
/// going missing until someone remembers to extend this list.
pub fn action_window_for_kind(kind: &str) -> &str {
    match kind {
        "lane" => WORK_WINDOW_NAME,
        "review-fix" => FIX_WINDOW_NAME,
        "audit" => AUDIT_WINDOW_NAME,
        "bugbot-cleanup" => CLEANUP_WINDOW_NAME,
        other => other,
    }
}

/// One window [`reconstruct_session`] should create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedWindow {
    /// Window name, already deduplicated against the session's existing
    /// windows by [`unique_window_name`].
    pub name: String,
    /// Working directory to root the window at.
    pub dir: String,
    /// The pane command, or `None` for a plain shell.
    pub command: Option<String>,
    /// The run this window belongs to, or `None` for the session's
    /// [`SHELL_WINDOW_NAME`] window.
    pub run_id: Option<i64>,
    /// The `claude --resume` session id to tell the user about, set only for
    /// an interactive run whose `claude` process died with its pane. See the
    /// module docs on why this is a printed hint and not a command.
    pub resume_session_id: Option<String>,
}

/// What [`reconstruct_session`] is going to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPlan {
    /// The ticket's session name (`tm-<lowercased key>`).
    pub session_name: String,
    /// Whether the session already exists — a session always has at least
    /// one window, so an empty window list means it does not.
    pub session_exists: bool,
    /// Windows to create, in order. Empty means there is nothing to do.
    pub windows: Vec<PlannedWindow>,
}

/// Plan the reconstruction of `session_key`'s ticket session from `runs`
/// (chronological, as [`crate::runs::RunStore::runs_for_ticket`] returns
/// them) and `windows`, a [`TmuxOps::list_windows`] snapshot.
///
/// Pure, so the whole decision table above is testable against plain row and
/// name lists. `viewer_command` builds a headless run's follow command
/// (injected rather than called directly so this stays independent of how the
/// `tm` binary is addressed — see [`crate::work::viewer::viewer_command`]).
///
/// The `shell` window is planned last and rooted at the newest run's
/// worktree, which is the ticket's current worktree; windows are append-only,
/// so a `shell` that predates reconstruction keeps whatever root it had.
pub fn plan_session(
    session_key: &str,
    runs: &[Run],
    windows: &[crate::work::tmux::TmuxWindow],
    viewer_command: &dyn Fn(i64) -> String,
) -> SessionPlan {
    let session_name = ticket_session_name(session_key);
    let mut existing = session_window_names(windows, &session_name);
    let session_exists = !existing.is_empty();
    let mut planned = Vec::new();

    for run in runs.iter().filter(|run| !run.status.is_terminal()) {
        let action = action_window_for_kind(&run.kind);
        // A live window for this action is the action still being present,
        // whether it is `fix` or `fix-2` — the same rule the double-launch
        // guard uses.
        if has_live_window(windows, &session_name, action) {
            continue;
        }
        let name = unique_window_name(action, &existing);
        existing.push(name.clone());
        let is_headless = run.log_path.is_some();
        planned.push(PlannedWindow {
            name,
            dir: run.worktree.clone(),
            command: is_headless.then(|| viewer_command(run.id)),
            run_id: Some(run.id),
            resume_session_id: if is_headless {
                None
            } else {
                run.session_id.clone()
            },
        });
    }

    if !has_live_window(windows, &session_name, SHELL_WINDOW_NAME) {
        // The newest run's worktree, not the oldest: a ticket's audit run is
        // rooted in `[work.audit].dir` (pre-worktree) and its lane run in the
        // worktree, so taking the latest gives the shell the most useful root
        // available. A ticket with no runs at all never reaches here.
        if let Some(newest) = runs.last() {
            let name = unique_window_name(SHELL_WINDOW_NAME, &existing);
            planned.push(PlannedWindow {
                name,
                dir: newest.worktree.clone(),
                command: None,
                run_id: None,
                resume_session_id: None,
            });
        }
    }

    SessionPlan {
        session_name,
        session_exists,
        windows: planned,
    }
}

/// Execute `plan` against `tmux`: create the session with its first planned
/// window if it does not exist yet, append the rest, and select the first
/// window at the end (tmux's `new-window` steals focus).
///
/// A plan with no windows touches nothing — reconstruction of a healthy
/// session is a no-op, by design.
pub fn reconstruct_session(tmux: &dyn TmuxOps, plan: &SessionPlan) -> Result<(), TmuxError> {
    let Some((first, rest)) = plan.windows.split_first() else {
        return Ok(());
    };

    let mut appended: &[PlannedWindow] = rest;
    if plan.session_exists {
        appended = &plan.windows;
    } else {
        match &first.command {
            Some(command) => tmux.new_session_with_command(
                &plan.session_name,
                &first.dir,
                &first.name,
                &[],
                command,
            )?,
            None => tmux.new_session(&plan.session_name, &first.dir, &first.name)?,
        }
    }

    for window in appended {
        match &window.command {
            Some(command) => tmux.new_window_with_command(
                &plan.session_name,
                &window.name,
                &window.dir,
                &[],
                command,
            )?,
            None => tmux.new_window(&plan.session_name, &window.name, &window.dir)?,
        }
    }

    tmux.select_window(&plan.session_name, &plan.windows[0].name)
}

/// Whether `status` is one a reconstructed window is created for. Exposed for
/// the same reason [`action_window_for_kind`] is: it is a decision, not an
/// implementation detail.
pub fn is_in_flight(status: RunStatus) -> bool {
    !status.is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::tmux::{FakeTmuxOps, TmuxCall, TmuxWindow};

    fn viewer(run_id: i64) -> String {
        format!("tm runs logs {run_id} --follow")
    }

    /// A [`Run`] with only the fields reconstruction reads set to anything
    /// meaningful.
    fn run(id: i64, kind: &str, status: RunStatus, log: Option<&str>) -> Run {
        Run {
            id,
            ticket: "PROJ-1".to_string(),
            lane: "mylane".to_string(),
            kind: kind.to_string(),
            status,
            session_id: None,
            worktree: "/wt/proj-1".to_string(),
            branch: Some("jowi-dev/proj-1".to_string()),
            pid: None,
            transcript: None,
            started_at: "2026-08-20 10:00:00".to_string(),
            heartbeat_at: None,
            ended_at: None,
            exit_code: None,
            num_turns: None,
            cost_usd: None,
            blocker: None,
            pr_url: None,
            age_secs: 0,
            model_usage: None,
            log_path: log.map(str::to_string),
            findings_count: None,
        }
    }

    fn window(name: &str, dead: bool) -> TmuxWindow {
        TmuxWindow {
            session: "tm-proj-1".to_string(),
            name: name.to_string(),
            dead,
        }
    }

    #[test]
    fn action_window_names_the_action_not_the_run_kind() {
        assert_eq!(action_window_for_kind("lane"), "work");
        assert_eq!(action_window_for_kind("review-fix"), "fix");
        assert_eq!(action_window_for_kind("audit"), "audit");
        assert_eq!(action_window_for_kind("bugbot-cleanup"), "bugbot");
    }

    #[test]
    fn action_window_falls_back_to_an_unknown_kind_verbatim() {
        assert_eq!(action_window_for_kind("review-watch"), "review-watch");
    }

    #[test]
    fn a_live_headless_run_is_rebuilt_as_a_log_viewer() {
        let runs = vec![run(7, "lane", RunStatus::Running, Some("/state/a.log"))];

        let plan = plan_session("PROJ-1", &runs, &[], &viewer);

        assert_eq!(plan.session_name, "tm-proj-1");
        assert!(!plan.session_exists);
        assert_eq!(
            plan.windows,
            vec![
                PlannedWindow {
                    name: "work".to_string(),
                    dir: "/wt/proj-1".to_string(),
                    command: Some("tm runs logs 7 --follow".to_string()),
                    run_id: Some(7),
                    resume_session_id: None,
                },
                PlannedWindow {
                    name: "shell".to_string(),
                    dir: "/wt/proj-1".to_string(),
                    command: None,
                    run_id: None,
                    resume_session_id: None,
                },
            ]
        );
    }

    /// An interactive run's `claude` lived in the pane that just died. The
    /// window comes back as a shell in its worktree, carrying the session id
    /// so the caller can print a `claude --resume` line — never running it.
    #[test]
    fn a_live_interactive_run_is_rebuilt_as_a_shell_with_a_resume_hint() {
        let mut lane = run(7, "lane", RunStatus::Running, None);
        lane.session_id = Some("sess-abc".to_string());
        let runs = vec![lane];

        let plan = plan_session("PROJ-1", &runs, &[], &viewer);

        assert_eq!(plan.windows[0].name, "work");
        assert_eq!(
            plan.windows[0].command, None,
            "nothing can resurrect a dead claude pane, and auto-resuming \
             would start billing and editing unasked"
        );
        assert_eq!(
            plan.windows[0].resume_session_id,
            Some("sess-abc".to_string())
        );
    }

    /// The phase-4 question the issue leaves open: a *finished* run — of
    /// either hosting — is history, and history lives in the run table and
    /// the log files, not in rebuilt tmux windows.
    #[test]
    fn finished_runs_get_no_window_of_either_kind() {
        let runs = vec![
            run(1, "audit", RunStatus::Done, None),
            run(2, "lane", RunStatus::Failed, Some("/state/a.log")),
            run(3, "review-fix", RunStatus::Interrupted, None),
        ];

        let plan = plan_session("PROJ-1", &runs, &[], &viewer);

        assert_eq!(
            plan.windows.iter().map(|w| &w.name).collect::<Vec<_>>(),
            vec!["shell"],
            "only the session's shell window is worth rebuilding"
        );
    }

    #[test]
    fn every_in_flight_run_gets_its_own_window_in_chronological_order() {
        let runs = vec![
            run(1, "audit", RunStatus::Done, None),
            run(2, "lane", RunStatus::Running, Some("/state/a.log")),
            run(3, "review-fix", RunStatus::Queued, Some("/state/b.log")),
        ];

        let plan = plan_session("PROJ-1", &runs, &[], &viewer);

        assert_eq!(
            plan.windows.iter().map(|w| &w.name).collect::<Vec<_>>(),
            vec!["work", "fix", "shell"]
        );
    }

    #[test]
    fn a_healthy_session_is_left_alone() {
        let runs = vec![run(7, "lane", RunStatus::Running, Some("/state/a.log"))];
        let windows = vec![window("work", false), window("shell", false)];

        let plan = plan_session("PROJ-1", &runs, &windows, &viewer);

        assert!(plan.session_exists);
        assert!(
            plan.windows.is_empty(),
            "reconstruction must be safe to run against a live session"
        );
    }

    #[test]
    fn a_live_repeat_window_counts_as_the_action_being_present() {
        // `fix-2`'s action is still `fix`, so the pass is already on screen.
        let runs = vec![run(7, "review-fix", RunStatus::Running, Some("/s/a.log"))];
        let windows = vec![window("fix-2", false), window("shell", false)];

        let plan = plan_session("PROJ-1", &runs, &windows, &viewer);

        assert!(plan.windows.is_empty());
    }

    #[test]
    fn a_dead_window_is_rebuilt_under_the_next_free_name() {
        let runs = vec![run(7, "lane", RunStatus::Running, Some("/state/a.log"))];
        let windows = vec![window("work", true), window("shell", false)];

        let plan = plan_session("PROJ-1", &runs, &windows, &viewer);

        assert_eq!(
            plan.windows.iter().map(|w| &w.name).collect::<Vec<_>>(),
            vec!["work-2"],
            "the dead window's name is not reused, and shell is already live"
        );
    }

    #[test]
    fn a_ticket_with_no_runs_plans_nothing() {
        let plan = plan_session("PROJ-1", &[], &[], &viewer);

        assert!(plan.windows.is_empty());
    }

    #[test]
    fn the_shell_window_is_rooted_at_the_newest_runs_worktree() {
        let mut audit = run(1, "audit", RunStatus::Done, None);
        audit.worktree = "/repo/axiom".to_string();
        let mut lane = run(2, "lane", RunStatus::Done, Some("/s/a.log"));
        lane.worktree = "/wt/proj-1".to_string();

        let plan = plan_session("PROJ-1", &[audit, lane], &[], &viewer);

        assert_eq!(plan.windows[0].name, "shell");
        assert_eq!(plan.windows[0].dir, "/wt/proj-1");
    }

    #[test]
    fn reconstruct_creates_the_session_from_the_first_planned_window() {
        let runs = vec![run(7, "lane", RunStatus::Running, Some("/state/a.log"))];
        let plan = plan_session("PROJ-1", &runs, &[], &viewer);
        let tmux = FakeTmuxOps::new();

        reconstruct_session(&tmux, &plan).unwrap();

        assert_eq!(
            tmux.calls(),
            vec![
                TmuxCall::NewSessionWithCommand {
                    name: "tm-proj-1".to_string(),
                    dir: "/wt/proj-1".to_string(),
                    window_name: "work".to_string(),
                    env: Vec::new(),
                    command: "tm runs logs 7 --follow".to_string(),
                },
                TmuxCall::NewWindow {
                    name: "tm-proj-1".to_string(),
                    window_name: "shell".to_string(),
                    dir: "/wt/proj-1".to_string(),
                },
                TmuxCall::SelectWindow {
                    name: "tm-proj-1".to_string(),
                    window: "work".to_string(),
                },
            ]
        );
    }

    #[test]
    fn reconstruct_appends_every_window_when_the_session_survives() {
        let runs = vec![run(7, "lane", RunStatus::Running, Some("/state/a.log"))];
        let windows = vec![window("shell", false)];
        let plan = plan_session("PROJ-1", &runs, &windows, &viewer);
        let tmux = FakeTmuxOps::new();

        reconstruct_session(&tmux, &plan).unwrap();

        assert_eq!(
            tmux.calls(),
            vec![
                TmuxCall::NewWindowWithCommand {
                    name: "tm-proj-1".to_string(),
                    window_name: "work".to_string(),
                    dir: "/wt/proj-1".to_string(),
                    env: Vec::new(),
                    command: "tm runs logs 7 --follow".to_string(),
                },
                TmuxCall::SelectWindow {
                    name: "tm-proj-1".to_string(),
                    window: "work".to_string(),
                },
            ]
        );
    }

    #[test]
    fn reconstruct_creates_a_shell_only_session_when_nothing_is_in_flight() {
        let runs = vec![run(1, "lane", RunStatus::Done, Some("/s/a.log"))];
        let plan = plan_session("PROJ-1", &runs, &[], &viewer);
        let tmux = FakeTmuxOps::new();

        reconstruct_session(&tmux, &plan).unwrap();

        assert_eq!(
            tmux.calls(),
            vec![
                TmuxCall::NewSession {
                    name: "tm-proj-1".to_string(),
                    dir: "/wt/proj-1".to_string(),
                    primary_window: "shell".to_string(),
                },
                TmuxCall::SelectWindow {
                    name: "tm-proj-1".to_string(),
                    window: "shell".to_string(),
                },
            ]
        );
    }

    #[test]
    fn reconstruct_touches_nothing_for_an_empty_plan() {
        let plan = plan_session("PROJ-1", &[], &[], &viewer);
        let tmux = FakeTmuxOps::new();

        reconstruct_session(&tmux, &plan).unwrap();

        assert!(tmux.calls().is_empty());
    }

    #[test]
    fn in_flight_covers_exactly_the_non_terminal_statuses() {
        assert!(is_in_flight(RunStatus::Running));
        assert!(is_in_flight(RunStatus::Queued));
        assert!(!is_in_flight(RunStatus::Done));
        assert!(!is_in_flight(RunStatus::Failed));
    }
}
