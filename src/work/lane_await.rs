//! Finishing an unattended interactive lane on its done marker (GitHub
//! issue #184).
//!
//! `tm work run --unattended` (the autopilot dispatcher's launch) runs its
//! lane interactively in the ticket session's `work` window, where claude
//! sits at its prompt once it is done instead of exiting. The lane's prompt
//! ends with [`lane_completion_section`], telling the agent to write a done
//! marker once its pull request is open. A detached `tm work __await-lane`
//! process runs [`await_lane`]: it waits for that marker, then finishes the
//! run row and runs the same post-PR tail the headless supervisor does
//! (`crate::work::run::supervise_run`): `status_on_pr`, `tm pr watch` and,
//! under autopilot, `tm review auto`.

use std::cell::Cell;
use std::io::{self, Write};
use std::path::Path;

use thiserror::Error;

use crate::github::gh_cli::GhCli;
use crate::runs::{FinishRun, RunStatus, RunStore, RunStoreError};
use crate::ticketing::provider::TicketProvider;
use crate::work::review_watch::Sleeper;
use crate::work::run::{
    AutoReviewStarter, Clock, PreparedRun, RunLaneError, WatchArmer, apply_status_on_pr,
    post_pr_tail, resolve_pr_url,
};
use crate::work::signal::{
    Signal, StepState, StepWatch, read_done_marker, temporary_name, wait_for_signal,
};
use crate::work::tmux::{TmuxError, TmuxOps};

/// Event kind [`await_lane`] records on the lane's run row when it starts,
/// its detail the awaiter's own pid. The row's pid is its agent's, so this
/// is the only record of which process is waiting on the lane.
pub const LANE_AWAIT_PID_EVENT: &str = "lane_await_pid";

/// Event kind [`await_lane`] records on the lane's run row when it stops
/// waiting. Its detail is `succeeded`, `stopped without a pull request`, or
/// [`GONE_DETAIL`].
pub const LANE_DONE_EVENT: &str = "lane_done";

/// [`LANE_DONE_EVENT`]'s detail when [`await_lane`] stops with no done
/// marker: the row is missing, its tmux session is gone, or it stayed ended
/// for [`ENDED_GRACE_SECS`] without the marker landing.
pub const GONE_DETAIL: &str =
    "stopped waiting: the row ended or its session is gone, no done marker";

/// How long [`await_lane`] keeps polling for the marker after the row has
/// ended. An agent that runs `tm runs finish` itself writes its marker a
/// moment later, usually more than one poll apart, and that marker should
/// still finish the lane.
pub const ENDED_GRACE_SECS: i64 = 120;

/// The blocker [`await_lane`] records on a lane whose agent signalled
/// `{"succeeded": false}`.
pub const NO_PR_BLOCKER: &str = "the lane stopped without a pull request";

/// The section tm appends to an unattended interactive lane's prompt: tm
/// waits for `marker` ([`crate::work::run::PreparedRun::done_marker`]),
/// written last under a temporary name and renamed into place.
pub fn lane_completion_section(marker: &Path) -> String {
    let tmp = temporary_name(marker);
    format!(
        "\n\n## Completion signal\n\n\
         tm is waiting for `{file}` to know this lane is done. Once the pull \
         request is open, write `{{\"succeeded\": true}}` to `{tmp}`, or \
         `{{\"succeeded\": false}}` if you are stopping without a pull \
         request (blocked, or you cannot finish), then rename it into place \
         with `mv '{tmp}' '{file}'`, so tm never reads a half-written file. \
         Then stop and leave this session open: a human may attach to it.\n",
        file = marker.display(),
        tmp = tmp.display(),
    )
}

/// Everything [`await_lane`] talks to, behind traits so it is tested with
/// fakes.
pub struct AwaitLaneDeps<'a> {
    /// The run store holding the lane's row.
    pub run_store: &'a RunStore,
    /// Looks up the lane branch's pull request.
    pub gh: &'a dyn GhCli,
    /// Applies `status_on_pr`; `None` when no provider could be built.
    pub ticket_provider: Option<&'a dyn TicketProvider>,
    /// Asks whether the lane's tmux session still exists.
    pub tmux: &'a dyn TmuxOps,
    /// Waits between two polls for the done marker.
    pub sleeper: &'a dyn Sleeper,
    /// "Now", for [`wait_for_signal`]'s (unbounded) timeout.
    pub clock: &'a dyn Clock,
    /// Arms `tm pr watch`; `Some` when `[work.review_watch].auto_arm` is on.
    pub watch_armer: Option<&'a dyn WatchArmer>,
    /// Starts `tm review auto` when the scope's autopilot flag is on.
    pub review_starter: Option<&'a dyn AutoReviewStarter>,
    /// The awaiter's own pid, recorded as [`LANE_AWAIT_PID_EVENT`].
    pub pid: u32,
}

/// How [`await_lane`] stopped waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AwaitOutcome {
    /// The agent wrote its done marker, and the row was finished.
    Done {
        /// The marker's `succeeded` value.
        succeeded: bool,
    },
    /// No marker: the row is missing, its session went away, or it stayed
    /// ended past [`ENDED_GRACE_SECS`]. Nothing was touched.
    Gone,
}

/// Errors from [`await_lane`].
#[derive(Debug, Error)]
pub enum AwaitLaneError {
    /// The prepared run has no [`PreparedRun::done_marker`]: it was not an
    /// unattended interactive lane, so there is nothing to wait for.
    #[error("run {0} has no done marker to wait for")]
    NoDoneMarker(i64),
    /// Reading or writing the run store failed.
    #[error(transparent)]
    RunStore(#[from] RunStoreError),
    /// Asking tmux about the lane's session failed.
    #[error(transparent)]
    Tmux(#[from] TmuxError),
    /// Arming the watch or starting the review failed.
    #[error(transparent)]
    Lane(#[from] RunLaneError),
    /// Writing to the log failed.
    #[error("io error: {0}")]
    Io(#[from] io::Error),
}

/// Waits for an unattended interactive lane's done marker, then finishes
/// its run row (GitHub issue #184).
///
/// Records [`LANE_AWAIT_PID_EVENT`] first, then polls the marker with no
/// timeout: an autopilot lane has no deadline, and its row stays `running`
/// (counting toward `max_lanes`) until the marker, the row ending, or the
/// session going away (see [`LaneWatch`]). On the marker the row is
/// finished `done`, or `blocked` with [`NO_PR_BLOCKER`] for
/// `{"succeeded": false}`, with the branch's PR URL, through
/// [`RunStore::finish_run_from_supervisor`] without forcing, so a status the
/// agent already set with `tm runs finish` wins. Then `status_on_pr`,
/// `tm pr watch` and `tm review auto` run as they do after a headless lane.
/// A row that ends first ([`AwaitOutcome::Gone`]) is left as it is.
pub fn await_lane(
    deps: &AwaitLaneDeps<'_>,
    prepared: &PreparedRun,
    out: &mut dyn Write,
) -> Result<AwaitOutcome, AwaitLaneError> {
    let run_id = prepared.run_id;
    let Some(marker) = prepared.done_marker.as_deref() else {
        return Err(AwaitLaneError::NoDoneMarker(run_id));
    };
    deps.run_store
        .add_event(run_id, LANE_AWAIT_PID_EVENT, Some(&deps.pid.to_string()))?;

    let watch = LaneWatch {
        deps,
        ended_since: Cell::new(None),
    };
    let signal = wait_for_signal(&watch, run_id, i64::MAX, || read_done_marker(marker))?;
    let succeeded = match signal {
        Signal::Received(succeeded) => succeeded,
        Signal::Gone => {
            deps.run_store
                .add_event(run_id, LANE_DONE_EVENT, Some(GONE_DETAIL))?;
            writeln!(
                out,
                "lane awaiter {GONE_DETAIL}; leaving its run row as it is"
            )?;
            return Ok(AwaitOutcome::Gone);
        }
        Signal::Stuck => unreachable!(
            "the timeout is i64::MAX and LaneWatch reports a hibernated lane as running"
        ),
    };

    let pr_url = resolve_pr_url(deps.gh, &prepared.branch, None);
    let has_pr = pr_url.is_some();
    // A row the agent already ended with `tm runs finish` keeps its status
    // (no forcing below) and its own blocker text: `blocker` is
    // COALESCEd, so passing ours would overwrite the agent's.
    let already_ended = deps
        .run_store
        .run_by_id(run_id)?
        .is_some_and(|run| run.ended_at.is_some());
    deps.run_store.finish_run_from_supervisor(
        run_id,
        &FinishRun {
            status: if succeeded {
                RunStatus::Done
            } else {
                RunStatus::Blocked
            },
            blocker: (!succeeded && !already_ended).then(|| NO_PR_BLOCKER.to_string()),
            pr_url,
            ..FinishRun::default()
        },
        false,
    )?;
    let detail = if succeeded {
        "succeeded"
    } else {
        "stopped without a pull request"
    };
    deps.run_store
        .add_event(run_id, LANE_DONE_EVENT, Some(detail))?;
    writeln!(out, "lane done: {detail}")?;

    // What a headless lane gets from `run_agent_and_finish` and
    // `supervise_run` once its row is finished.
    apply_status_on_pr(deps.ticket_provider, prepared, has_pr, out)?;
    post_pr_tail(
        deps.run_store,
        deps.watch_armer,
        deps.review_starter,
        prepared,
        out,
    )?;
    Ok(AwaitOutcome::Done { succeeded })
}

/// The [`StepWatch`] [`await_lane`] polls through.
struct LaneWatch<'a, 'b> {
    deps: &'a AwaitLaneDeps<'b>,
    /// When a poll first saw the row ended, for [`ENDED_GRACE_SECS`].
    ended_since: Cell<Option<i64>>,
}

impl StepWatch for LaneWatch<'_, '_> {
    type Error = AwaitLaneError;

    /// The lane's own idea of [`StepState::Gone`]: a missing row or a dead
    /// tmux session, or a row that has stayed ended for
    /// [`ENDED_GRACE_SECS`]. Two differences from
    /// [`crate::work::signal::step_state_of`]:
    ///
    /// - A hibernated lane counts as running. `tm review auto` gives up on
    ///   a hibernated step, but a hibernated lane is resumed in place when
    ///   someone attaches to it (GitHub issue #64), and its agent can still
    ///   write the marker then.
    /// - An ended row is waited on for a grace period, since the agent may
    ///   run `tm runs finish` before it writes the marker.
    fn step_state(&self, run_id: i64) -> Result<StepState, AwaitLaneError> {
        let Some(run) = self.deps.run_store.run_by_id(run_id)? else {
            return Ok(StepState::Gone);
        };
        if let Some(session) = run.tmux_session.as_deref()
            && !self.deps.tmux.has_session(session)?
        {
            return Ok(StepState::Gone);
        }
        if matches!(
            run.status,
            RunStatus::Running | RunStatus::Queued | RunStatus::Hibernated
        ) {
            return Ok(StepState::Running);
        }
        let now = self.now_unix_secs();
        let since = self.ended_since.get().unwrap_or(now);
        self.ended_since.set(Some(since));
        Ok(if now - since < ENDED_GRACE_SECS {
            StepState::Running
        } else {
            StepState::Gone
        })
    }

    fn sleep(&self, secs: u64) {
        self.deps.sleeper.sleep(secs);
    }

    fn now_unix_secs(&self) -> i64 {
        self.deps.clock.now_unix_secs()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::*;
    use crate::agent::AgentInvocation;
    use crate::github::gh_cli::FakeGhCli;
    use crate::jira::fake::FakeJiraClient;
    use crate::runs::StartRun;
    use crate::ticketing::types::{Issue, IssueFields, Status, StatusCategory, Transition};
    use crate::work::tmux::FakeTmuxOps;

    const PR_URL: &str = "https://github.com/example/repo/pull/7";
    const SCOPE: &str = "jira:example";

    /// A lane run row in a fresh store and the [`PreparedRun`] an
    /// unattended interactive launch would hand the awaiter.
    struct Fixture {
        _tmp: TempDir,
        store: RunStore,
        prepared: PreparedRun,
        marker: PathBuf,
    }

    fn fixture() -> Fixture {
        let tmp = TempDir::new().unwrap();
        let store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let run_id = store
            .start_run(&StartRun {
                ticket: "ABC-123".to_string(),
                scope: SCOPE.to_string(),
                lane: "mylane".to_string(),
                worktree: "/wt/abc-123".to_string(),
                branch: Some("jowi-dev/abc-123-slug".to_string()),
                pid: None,
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();
        let marker = tmp.path().join("abc-123-20260806-090503.done");
        let prepared = PreparedRun {
            run_id,
            lane: "mylane".to_string(),
            ticket: Some("ABC-123".to_string()),
            wt_name: "abc-123".to_string(),
            timestamp: "20260806-090503".to_string(),
            worktree: PathBuf::from("/wt/abc-123"),
            branch: "jowi-dev/abc-123-slug".to_string(),
            invocation: AgentInvocation {
                program: "claude".to_string(),
                args: vec!["do the thing".to_string()],
                env_set: Vec::new(),
                env_remove: Vec::new(),
            },
            out_json_path: tmp.path().join("abc-123-20260806-090503.json"),
            fallbacks: Vec::new(),
            status_on_pr: Some("In Review".to_string()),
            done_marker: Some(marker.clone()),
        };
        Fixture {
            _tmp: tmp,
            store,
            prepared,
            marker,
        }
    }

    /// A [`Sleeper`] that writes `marker` with `contents` on its `nth` sleep
    /// (never, when `nth` is 0), counting sleeps.
    struct MarkerSleeper {
        marker: PathBuf,
        contents: &'static str,
        nth: u32,
        sleeps: Cell<u32>,
    }

    impl MarkerSleeper {
        fn never() -> Self {
            Self::on(0, PathBuf::new(), "")
        }

        fn on(nth: u32, marker: PathBuf, contents: &'static str) -> Self {
            MarkerSleeper {
                marker,
                contents,
                nth,
                sleeps: Cell::new(0),
            }
        }
    }

    impl Sleeper for MarkerSleeper {
        fn sleep(&self, _secs: u64) {
            self.sleeps.set(self.sleeps.get() + 1);
            if self.sleeps.get() == self.nth {
                std::fs::write(&self.marker, self.contents).unwrap();
            }
        }
    }

    /// A [`Clock`] that moves one signal poll forward every time it is
    /// read, so [`ENDED_GRACE_SECS`] runs out.
    struct TickingClock(Cell<i64>);

    impl TickingClock {
        fn new() -> Self {
            TickingClock(Cell::new(1_000))
        }
    }

    impl Clock for TickingClock {
        fn now_parts(&self) -> (i32, u32, u32, u32, u32, u32) {
            (2026, 8, 6, 9, 5, 3)
        }

        fn now_unix_secs(&self) -> i64 {
            let now = self.0.get();
            self.0
                .set(now + crate::work::signal::SIGNAL_POLL_SECS as i64);
            now
        }
    }

    /// Records every ticket it is asked to arm or start, for both traits.
    #[derive(Default)]
    struct Recorder {
        calls: RefCell<Vec<String>>,
    }

    impl WatchArmer for Recorder {
        fn arm(&self, ticket: &str, _working_dir: &Path) -> Result<(), String> {
            self.calls.borrow_mut().push(ticket.to_string());
            Ok(())
        }
    }

    impl AutoReviewStarter for Recorder {
        fn start(&self, ticket: &str, _working_dir: &Path) -> Result<(), String> {
            self.calls.borrow_mut().push(ticket.to_string());
            Ok(())
        }
    }

    fn jira() -> FakeJiraClient {
        FakeJiraClient::new()
            .with_issue(
                "ABC-123",
                Issue {
                    key: "ABC-123".to_string(),
                    fields: IssueFields {
                        summary: "Delete bid connector".to_string(),
                        status: Status {
                            name: "In Progress".to_string(),
                            status_category: StatusCategory {
                                key: "indeterminate".to_string(),
                            },
                        },
                        description: None,
                        assignee: None,
                        issue_links: vec![],
                        parent: None,
                        labels: Vec::new(),
                        issue_type: None,
                    },
                },
            )
            .with_transitions(
                "ABC-123",
                vec![Transition {
                    id: "t-review".to_string(),
                    name: "Request review".to_string(),
                    to: Status {
                        name: "In Review".to_string(),
                        status_category: StatusCategory {
                            key: "indeterminate".to_string(),
                        },
                    },
                }],
            )
    }

    /// What one [`await_lane`] call saw and did.
    struct Awaited {
        outcome: AwaitOutcome,
        printed: String,
        armed: Vec<String>,
        reviewed: Vec<String>,
    }

    fn await_with(
        fx: &Fixture,
        tmux: &FakeTmuxOps,
        sleeper: &MarkerSleeper,
        provider: Option<&dyn TicketProvider>,
    ) -> Awaited {
        let gh = FakeGhCli::new().with_pr_url_for_branch(Ok(Some(PR_URL.to_string())));
        let armer = Recorder::default();
        let starter = Recorder::default();
        let deps = AwaitLaneDeps {
            run_store: &fx.store,
            gh: &gh,
            ticket_provider: provider,
            tmux,
            sleeper,
            clock: &TickingClock::new(),
            watch_armer: Some(&armer),
            review_starter: Some(&starter),
            pid: 4242,
        };
        let mut out = Vec::new();
        let outcome = await_lane(&deps, &fx.prepared, &mut out).unwrap();
        Awaited {
            outcome,
            printed: String::from_utf8(out).unwrap(),
            armed: armer.calls.take(),
            reviewed: starter.calls.take(),
        }
    }

    fn events(store: &RunStore, run_id: i64, kind: &str) -> Vec<Option<String>> {
        store
            .events_for_run(run_id)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == kind)
            .map(|e| e.detail)
            .collect()
    }

    fn write_marker(fx: &Fixture, contents: &str) {
        std::fs::write(&fx.marker, contents).unwrap();
    }

    #[test]
    fn a_succeeded_marker_finishes_the_row_done_and_runs_the_post_pr_tail() {
        let fx = fixture();
        write_marker(&fx, r#"{"succeeded": true}"#);
        let jira = jira();

        let got = await_with(
            &fx,
            &FakeTmuxOps::new(),
            &MarkerSleeper::never(),
            Some(&jira),
        );

        assert_eq!(got.outcome, AwaitOutcome::Done { succeeded: true });
        let run = fx.store.run_by_id(fx.prepared.run_id).unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Done);
        assert_eq!(run.pr_url.as_deref(), Some(PR_URL));
        assert_eq!(run.blocker, None);
        assert_eq!(
            jira.transition_calls(),
            vec![("ABC-123".to_string(), "t-review".to_string())]
        );
        assert!(
            got.printed.contains("moved ABC-123 to In Review"),
            "{}",
            got.printed
        );
        assert_eq!(got.armed, vec!["ABC-123".to_string()]);
        assert!(got.reviewed.is_empty(), "autopilot is off for the scope");
        assert_eq!(
            events(&fx.store, fx.prepared.run_id, LANE_AWAIT_PID_EVENT),
            vec![Some("4242".to_string())]
        );
        assert_eq!(
            events(&fx.store, fx.prepared.run_id, LANE_DONE_EVENT),
            vec![Some("succeeded".to_string())]
        );
    }

    #[test]
    fn a_succeeded_marker_starts_the_review_when_autopilot_is_on() {
        let fx = fixture();
        fx.store.set_autopilot(SCOPE, true).unwrap();
        write_marker(&fx, r#"{"succeeded": true}"#);

        let got = await_with(&fx, &FakeTmuxOps::new(), &MarkerSleeper::never(), None);

        assert_eq!(got.reviewed, vec!["ABC-123".to_string()]);
        assert_eq!(got.armed, vec!["ABC-123".to_string()]);
    }

    #[test]
    fn a_failed_marker_finishes_the_row_blocked() {
        let fx = fixture();
        write_marker(&fx, r#"{"succeeded": false}"#);

        let got = await_with(&fx, &FakeTmuxOps::new(), &MarkerSleeper::never(), None);

        assert_eq!(got.outcome, AwaitOutcome::Done { succeeded: false });
        let run = fx.store.run_by_id(fx.prepared.run_id).unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Blocked);
        assert_eq!(run.blocker.as_deref(), Some(NO_PR_BLOCKER));
        assert_eq!(
            events(&fx.store, fx.prepared.run_id, LANE_DONE_EVENT),
            vec![Some("stopped without a pull request".to_string())]
        );
    }

    #[test]
    fn a_status_the_agent_set_itself_wins() {
        let fx = fixture();
        fx.store
            .finish_run(
                fx.prepared.run_id,
                &FinishRun {
                    status: RunStatus::Blocked,
                    blocker: Some("needs a schema decision".to_string()),
                    ..FinishRun::default()
                },
            )
            .unwrap();
        write_marker(&fx, r#"{"succeeded": false}"#);

        let got = await_with(&fx, &FakeTmuxOps::new(), &MarkerSleeper::never(), None);

        assert_eq!(got.outcome, AwaitOutcome::Done { succeeded: false });
        let run = fx.store.run_by_id(fx.prepared.run_id).unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Blocked);
        assert_eq!(run.blocker.as_deref(), Some("needs a schema decision"));
    }

    #[test]
    fn a_marker_written_after_the_agent_finished_its_own_row_still_counts() {
        let fx = fixture();
        fx.store
            .finish_run(
                fx.prepared.run_id,
                &FinishRun {
                    status: RunStatus::Blocked,
                    blocker: Some("needs a schema decision".to_string()),
                    ..FinishRun::default()
                },
            )
            .unwrap();
        let sleeper = MarkerSleeper::on(3, fx.marker.clone(), r#"{"succeeded": false}"#);

        let got = await_with(&fx, &FakeTmuxOps::new(), &sleeper, None);

        assert_eq!(got.outcome, AwaitOutcome::Done { succeeded: false });
        let run = fx.store.run_by_id(fx.prepared.run_id).unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Blocked);
        assert_eq!(run.blocker.as_deref(), Some("needs a schema decision"));
        assert_eq!(run.pr_url.as_deref(), Some(PR_URL));
    }

    #[test]
    fn a_hibernated_lane_keeps_the_awaiter_waiting_until_its_marker() {
        let fx = fixture();
        assert!(fx.store.hibernate_run(fx.prepared.run_id).unwrap());
        let sleeper = MarkerSleeper::on(3, fx.marker.clone(), r#"{"succeeded": true}"#);

        let got = await_with(&fx, &FakeTmuxOps::new(), &sleeper, None);

        assert_eq!(got.outcome, AwaitOutcome::Done { succeeded: true });
        assert_eq!(sleeper.sleeps.get(), 3);
        let run = fx.store.run_by_id(fx.prepared.run_id).unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Done);
    }

    #[test]
    fn a_row_ended_by_something_else_is_gone_and_left_alone() {
        let fx = fixture();
        fx.store
            .finish_run(
                fx.prepared.run_id,
                &FinishRun {
                    status: RunStatus::Interrupted,
                    ..FinishRun::default()
                },
            )
            .unwrap();

        let sleeper = MarkerSleeper::never();
        let got = await_with(&fx, &FakeTmuxOps::new(), &sleeper, None);

        assert_eq!(got.outcome, AwaitOutcome::Gone);
        assert!(sleeper.sleeps.get() > 0, "waited out the grace period");
        let run = fx.store.run_by_id(fx.prepared.run_id).unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Interrupted);
        assert_eq!(run.pr_url, None);
        assert!(got.armed.is_empty() && got.reviewed.is_empty());
        assert_eq!(
            events(&fx.store, fx.prepared.run_id, LANE_DONE_EVENT),
            vec![Some(GONE_DETAIL.to_string())]
        );
    }

    #[test]
    fn a_dead_session_is_gone_and_left_alone() {
        let fx = fixture();
        fx.store
            .update_tmux_session(fx.prepared.run_id, "tm-abc-123")
            .unwrap();
        let tmux = FakeTmuxOps::new().with_has_session(Ok(false));

        let got = await_with(&fx, &tmux, &MarkerSleeper::never(), None);

        assert_eq!(got.outcome, AwaitOutcome::Gone);
        let run = fx.store.run_by_id(fx.prepared.run_id).unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Running);
        assert!(got.armed.is_empty() && got.reviewed.is_empty());
        assert!(got.printed.contains(GONE_DETAIL), "{}", got.printed);
    }

    #[test]
    fn a_run_without_a_done_marker_is_an_error() {
        let mut fx = fixture();
        fx.prepared.done_marker = None;
        let gh = FakeGhCli::new();
        let deps = AwaitLaneDeps {
            run_store: &fx.store,
            gh: &gh,
            ticket_provider: None,
            tmux: &FakeTmuxOps::new(),
            sleeper: &MarkerSleeper::never(),
            clock: &TickingClock::new(),
            watch_armer: None,
            review_starter: None,
            pid: 1,
        };

        let err = await_lane(&deps, &fx.prepared, &mut Vec::new()).unwrap_err();

        assert!(matches!(err, AwaitLaneError::NoDoneMarker(id) if id == fx.prepared.run_id));
    }

    #[test]
    fn the_lane_completion_section_names_the_marker_and_its_temporary_name() {
        let marker = Path::new("/state/proj-1-20260806-090503.done");
        let section = lane_completion_section(marker);
        assert!(section.starts_with("\n\n## Completion signal\n\n"));
        assert!(section.contains("`/state/proj-1-20260806-090503.done`"));
        assert!(section.contains(
            "mv '/state/proj-1-20260806-090503.done.tmp' '/state/proj-1-20260806-090503.done'"
        ));
        assert!(section.contains(r#"{"succeeded": true}"#));
        assert!(section.contains(r#"{"succeeded": false}"#));
        assert!(section.contains("leave this session open"));
    }
}
