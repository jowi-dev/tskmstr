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

use std::io::{self, Write};
use std::path::Path;

use thiserror::Error;

use crate::github::gh_cli::GhCli;
use crate::runs::{FinishRun, RunStatus, RunStore, RunStoreError};
use crate::ticketing::provider::TicketProvider;
use crate::work::review_watch::Sleeper;
use crate::work::run::{
    AutoReviewStarter, Clock, PreparedRun, RunLaneError, WatchArmer, apply_status_on_pr,
    auto_arm_watch, autopilot_start_review, resolve_pr_url,
};
use crate::work::signal::{
    Signal, StepState, StepWatch, read_done_marker, step_state_of, temporary_name, wait_for_signal,
};
use crate::work::tmux::{TmuxError, TmuxOps};

/// Event kind [`await_lane`] records on the lane's run row when it starts,
/// its detail the awaiter's own pid. The row's pid is its agent's, so this
/// is the only record of which process is waiting on the lane.
pub const LANE_AWAIT_PID_EVENT: &str = "lane_await_pid";

/// Event kind [`await_lane`] records on the lane's run row when it stops
/// waiting. Its detail is `succeeded`, `stopped without a pull request`, or
/// `row ended before the done signal`.
pub const LANE_DONE_EVENT: &str = "lane_done";

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
    /// The row ended, or its session went away, before any marker: nothing
    /// was touched.
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
/// session going away. On the marker the row is finished `done`, or
/// `blocked` with [`NO_PR_BLOCKER`] for `{"succeeded": false}`, with the
/// branch's PR URL, through
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

    let signal = wait_for_signal(&LaneWatch { deps }, run_id, i64::MAX, || {
        read_done_marker(marker)
    })?;
    let succeeded = match signal {
        Signal::Received(succeeded) => succeeded,
        // `Stuck` needs a timeout or a hibernated step, and `LaneWatch`
        // reports neither, so it only appears here for completeness.
        Signal::Gone | Signal::Stuck => {
            let detail = "row ended before the done signal";
            deps.run_store
                .add_event(run_id, LANE_DONE_EVENT, Some(detail))?;
            writeln!(out, "lane done: {detail}; leaving its run row as it is")?;
            return Ok(AwaitOutcome::Gone);
        }
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

    // The post-PR tail `supervise_run` runs after a headless lane.
    apply_status_on_pr(deps.ticket_provider, prepared, has_pr, out)?;
    if let Some(armer) = deps.watch_armer {
        auto_arm_watch(deps.run_store, armer, prepared, out)?;
    }
    if let Some(starter) = deps.review_starter {
        autopilot_start_review(deps.run_store, starter, prepared, out)?;
    }
    Ok(AwaitOutcome::Done { succeeded })
}

/// The [`StepWatch`] [`await_lane`] polls through.
struct LaneWatch<'a, 'b> {
    deps: &'a AwaitLaneDeps<'b>,
}

impl StepWatch for LaneWatch<'_, '_> {
    type Error = AwaitLaneError;

    /// [`step_state_of`] the lane's row, except that a hibernated lane
    /// counts as running. `tm review auto` gives up on a hibernated step,
    /// but a hibernated lane is resumed in place when someone attaches to
    /// it (GitHub issue #64), and its agent can still write the marker
    /// then, so the awaiter keeps waiting through hibernation.
    fn step_state(&self, run_id: i64) -> Result<StepState, AwaitLaneError> {
        let run = self.deps.run_store.run_by_id(run_id)?;
        let state = step_state_of(run.as_ref(), |session| self.deps.tmux.has_session(session))?;
        Ok(match state {
            StepState::Hibernated => StepState::Running,
            other => other,
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

    struct FixedClock;

    impl Clock for FixedClock {
        fn now_parts(&self) -> (i32, u32, u32, u32, u32, u32) {
            (2026, 8, 6, 9, 5, 3)
        }

        fn now_unix_secs(&self) -> i64 {
            1_000
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
            clock: &FixedClock,
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

        let got = await_with(&fx, &FakeTmuxOps::new(), &MarkerSleeper::never(), None);

        assert_eq!(got.outcome, AwaitOutcome::Gone);
        let run = fx.store.run_by_id(fx.prepared.run_id).unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Interrupted);
        assert_eq!(run.pr_url, None);
        assert!(got.armed.is_empty() && got.reviewed.is_empty());
        assert_eq!(
            events(&fx.store, fx.prepared.run_id, LANE_DONE_EVENT),
            vec![Some("row ended before the done signal".to_string())]
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
            clock: &FixedClock,
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
