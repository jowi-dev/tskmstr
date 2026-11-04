//! Starting a fix pass when a lane PR's CI fails (GitHub issue #226).
//!
//! Once a lane has opened its pull request, autopilot watches the PR's
//! checks. When every check on the PR's head commit has finished and at
//! least one failed, the dispatcher starts `tm review ci-fix` for the
//! ticket: a fix session in the ticket's worktree whose prompt carries the
//! failing checks' logs ([`build_prompt`]). A check still running holds
//! the trigger back, so a re-run in progress never starts a second pass.
//!
//! One fix pass per red head commit. [`sweep`] records a [`CI_FIX_EVENT`]
//! on the ticket's lane run naming the head SHA before it launches, so the
//! next tick sees the commit as [`handled`] even while the launched process
//! still waits on admission. A push that goes red again has a new SHA and
//! gets its own pass.
//!
//! CI fixes are `review-fix` runs, so they count against `[dispatch]
//! max_fix_rounds` with every other fix pass ([`crate::work::fix_cap`]).
//! At the cap, [`sweep`] starts nothing, records the commit as `capped`, and
//! flags the lane run for a human. The fix-cap desktop notice in
//! [`crate::work::notify::ticket_notices`] then fires on its own.
//!
//! The pure parts ([`ready_to_fix`], [`handled`], [`trim_log`],
//! [`build_prompt`]) do no I/O. [`collect_failures`] and [`sweep`] take
//! their `gh`, run store, and launcher as injected dependencies.

use std::collections::HashMap;
use std::path::Path;

use crate::github::gh_cli::{ChecksState, GhCli, PrChecks, PrLifecycle};
use crate::github::pr::pr_number_from_url;
use crate::runs::overview::OverviewRun;
use crate::runs::{RunEvent, RunStatus, RunStore, RunStoreError};

/// Event kind recorded on the ticket's lane run. Detail is `started <sha>`,
/// `capped <sha>`, or `failed <sha>: <error>`.
pub const CI_FIX_EVENT: &str = "ci_fix";

/// The most failure-log bytes a fix prompt carries, across every failing
/// check.
pub const LOG_LIMIT_BYTES: usize = 24_000;

/// Whether `checks` call for a CI fix: at least one check failed in a way
/// a code fix can address ([`crate::github::gh_cli::CheckResult::failed_for_fix`]) and none is
/// still pending. A re-run in progress, checks that pass, and checks that
/// are red only because they were cancelled or wait for approval never
/// trigger. The PR's lifecycle is not considered here.
pub fn ready_to_fix(checks: &PrChecks) -> bool {
    let mut failing = false;
    for check in &checks.checks {
        if check.state == ChecksState::Pending {
            return false;
        }
        failing |= check.failed_for_fix();
    }
    failing
}

/// Whether `events` (a lane run's, from [`RunStore::events_for_run`])
/// already hold a [`CI_FIX_EVENT`] for head commit `sha`, whatever its
/// outcome: `started`, `capped`, or `failed`.
pub fn handled(events: &[RunEvent], sha: &str) -> bool {
    events
        .iter()
        .filter(|event| event.kind == CI_FIX_EVENT)
        .filter_map(|event| event.detail.as_deref()?.split_whitespace().nth(1))
        .any(|word| word.trim_end_matches(':') == sha)
}

/// One failing check and its failure log (or why there is none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedCheck {
    /// The check's display name.
    pub name: String,
    /// The check's link, when `gh` reported one.
    pub details_url: Option<String>,
    /// The failed steps' log, or why it could not be fetched.
    pub log: Result<String, String>,
}

/// The last `limit` bytes of `log`, since CI logs end with the error.
///
/// The cut lands on a char boundary and, when the kept tail has one, on the
/// next line start. A trimmed log starts with a `[... N bytes trimmed ...]`
/// line; a log within `limit` comes back unchanged.
pub fn trim_log(log: &str, limit: usize) -> String {
    if log.len() <= limit {
        return log.to_string();
    }
    let mut start = log.len() - limit;
    while !log.is_char_boundary(start) {
        start += 1;
    }
    if let Some(newline) = log[start..].find('\n') {
        let line_start = start + newline + 1;
        if line_start < log.len() {
            start = line_start;
        }
    }
    format!("[... {start} bytes trimmed ...]\n{}", &log[start..])
}

/// The fix-session prompt for pull request `pr_url` whose head commit
/// `head_sha` failed the `failed` checks.
///
/// Lists the failing checks, then gives each one a `## <name>` section with
/// its log, trimmed so the logs together stay within [`LOG_LIMIT_BYTES`],
/// and ends with the instructions: fix the cause on the current branch,
/// run the checks locally, commit, and push.
pub fn build_prompt(pr_url: &str, head_sha: &str, failed: &[FailedCheck]) -> String {
    let with_logs = failed.iter().filter(|check| check.log.is_ok()).count();
    let per_check = LOG_LIMIT_BYTES / with_logs.max(1);

    let mut prompt = format!(
        "CI checks failed on this branch's pull request, {pr_url}, at head \
         commit {head_sha}. The failing checks:\n\n"
    );
    for check in failed {
        match &check.details_url {
            Some(url) => prompt.push_str(&format!("- {} ({url})\n", check.name)),
            None => prompt.push_str(&format!("- {}\n", check.name)),
        }
    }
    for check in failed {
        prompt.push_str(&format!("\n## {}\n\n", check.name));
        match &check.log {
            Ok(log) => {
                let log = trim_log(log, per_check);
                let fence = fence_for(&log);
                prompt.push_str(&format!("{fence}\n{}\n{fence}\n", log.trim_end()));
            }
            Err(error) => prompt.push_str(&format!("Could not fetch the log: {error}\n")),
        }
    }
    prompt.push_str(
        "\nFind and fix the cause of each failure on the current branch. \
         You are already on the branch this pull request was opened from; \
         do not create a new branch or worktree. Run the project's checks \
         locally to confirm they pass, commit the fix, and push the branch \
         so CI runs again.\n",
    );
    prompt
}

/// A Markdown code fence longer than any backtick run in `text`, so a log
/// that prints a fence of its own cannot close the block early.
fn fence_for(text: &str) -> String {
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    "`".repeat((longest + 1).max(3))
}

/// One [`FailedCheck`] per check in `checks` that
/// [`crate::github::gh_cli::CheckResult::failed_for_fix`], with its log
/// fetched in `dir`.
///
/// A check whose link names its Actions job gets that job's log alone
/// ([`GhCli::job_failed_log`]). `gh run view <run> --log-failed` prints
/// every failed job of the run, so fetching per run would hand each of
/// several failing jobs the same combined log and spend the prompt's log
/// budget on copies. A check linked only to its run falls back to
/// [`GhCli::run_failed_log`], fetched once per run. A check that is not an
/// Actions run, or whose log `gh` could not fetch, carries the reason
/// instead.
pub fn collect_failures(gh: &dyn GhCli, dir: &Path, checks: &PrChecks) -> Vec<FailedCheck> {
    let mut logs: HashMap<u64, Result<String, String>> = HashMap::new();
    checks
        .checks
        .iter()
        .filter(|check| check.failed_for_fix())
        .map(|check| {
            let log = match (check.job_id, check.run_id) {
                (Some(job_id), _) => gh
                    .job_failed_log(dir, job_id)
                    .map_err(|err| err.to_string()),
                (None, Some(run_id)) => logs
                    .entry(run_id)
                    .or_insert_with(|| {
                        gh.run_failed_log(dir, run_id)
                            .map_err(|err| err.to_string())
                    })
                    .clone(),
                (None, None) => Err(match &check.details_url {
                    Some(url) => format!("not a GitHub Actions run; see {url}"),
                    None => "not a GitHub Actions run, and no details link".to_string(),
                }),
            };
            FailedCheck {
                name: check.name.clone(),
                details_url: check.details_url.clone(),
                log,
            }
        })
        .collect()
}

/// Starts `tm review ci-fix` for a ticket. Production spawns it detached.
pub trait CiFixLauncher {
    /// Starts the CI fix pass for `ticket`.
    ///
    /// # Errors
    ///
    /// Why the pass could not be started, as text for the
    /// [`CI_FIX_EVENT`] detail.
    fn launch(&self, ticket: &str) -> Result<(), String>;
}

/// What [`sweep`] needs besides the runs it inspects.
pub struct SweepDeps<'a> {
    /// The run store holding the lane runs' events.
    pub store: &'a RunStore,
    /// Reads the PRs' checks.
    pub gh: &'a dyn GhCli,
    /// Where `gh` runs: a checkout of the scope's repo.
    pub repo_dir: &'a Path,
    /// Starts each fix pass.
    pub launcher: &'a dyn CiFixLauncher,
}

/// What [`sweep`] did for one ticket. Tickets it skipped have none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CiFixAction {
    /// A fix pass was started for head commit `sha`.
    Started {
        /// Ticket key.
        ticket: String,
        /// The failing head commit.
        sha: String,
    },
    /// The ticket is at its fix-round cap, so the lane run was flagged for
    /// review instead.
    Capped {
        /// Ticket key.
        ticket: String,
        /// The failing head commit.
        sha: String,
    },
    /// The launcher refused; recorded as a `failed` event, not retried for
    /// this commit.
    LaunchFailed {
        /// Ticket key.
        ticket: String,
        /// The failing head commit.
        sha: String,
        /// The launcher's error.
        error: String,
    },
    /// `gh` could not read the PR's checks; retried next sweep.
    CheckFailed {
        /// Ticket key.
        ticket: String,
        /// The `gh` error.
        error: String,
    },
}

/// One dispatch tick's CI-fix pass over `scope`: starts a fix for each
/// ticket whose lane PR is open and red on a head commit not yet handled.
/// A lane PR `gh` reports merged or closed gets a `pr_closed` event on its
/// lane run, so later sweeps skip the ticket without asking `gh` again.
///
/// `runs` is [`RunStore::overview_runs`] output, newest first. A ticket is
/// skipped while any of its runs is live (a lane still working, or a fix
/// already in flight), once any run recorded its PR closed, and when it has
/// no lane run with a PR. A ticket at `max_fix_rounds` is flagged for
/// review instead of fixed.
///
/// # Errors
///
/// Any run-store failure. A `gh` failure is a [`CiFixAction::CheckFailed`]
/// for that ticket and a launch failure a [`CiFixAction::LaunchFailed`];
/// neither stops the sweep.
pub fn sweep(
    deps: &SweepDeps<'_>,
    runs: &[OverviewRun],
    scope: &str,
    max_fix_rounds: u32,
) -> Result<Vec<CiFixAction>, RunStoreError> {
    let mut by_ticket: Vec<(&str, Vec<&OverviewRun>)> = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();
    for run in runs.iter().filter(|run| run.scope == scope) {
        let at = *index.entry(&run.ticket).or_insert_with(|| {
            by_ticket.push((&run.ticket, Vec::new()));
            by_ticket.len() - 1
        });
        by_ticket[at].1.push(run);
    }

    let mut actions = Vec::new();
    for (ticket, runs) in by_ticket {
        let live = runs.iter().any(|run| {
            matches!(
                run.status,
                RunStatus::Running | RunStatus::Queued | RunStatus::Hibernated
            )
        });
        if live || runs.iter().any(|run| run.pr_closed) {
            continue;
        }
        let Some((lane, number)) = runs.iter().find_map(|run| {
            let url = run.pr_url.as_deref().filter(|_| run.kind == "lane")?;
            Some((*run, pr_number_from_url(url)?))
        }) else {
            continue;
        };

        let checks = match deps.gh.pr_checks(deps.repo_dir, number) {
            Ok(checks) => checks,
            Err(err) => {
                actions.push(CiFixAction::CheckFailed {
                    ticket: ticket.to_string(),
                    error: err.to_string(),
                });
                continue;
            }
        };
        let reason = match checks.lifecycle {
            PrLifecycle::Open => None,
            PrLifecycle::Merged => Some("merged"),
            PrLifecycle::Closed => Some("closed"),
        };
        if let Some(reason) = reason {
            // Only `tm merge` and the review watcher record `pr_closed`
            // otherwise, so a PR merged or closed on GitHub would cost a
            // `gh` call on every later tick. Recording it here skips the
            // ticket from the next tick on.
            let detail = serde_json::json!({ "reason": reason }).to_string();
            deps.store.add_event(lane.id, "pr_closed", Some(&detail))?;
            continue;
        }
        let red = ready_to_fix(&checks);
        let sha = checks.head_sha;
        // An empty SHA would make every event look unhandled.
        if !red || sha.is_empty() || handled(&deps.store.events_for_run(lane.id)?, &sha) {
            continue;
        }

        if crate::work::fix_cap::fix_cap_reached(deps.store, scope, ticket, max_fix_rounds)? {
            deps.store
                .add_event(lane.id, CI_FIX_EVENT, Some(&format!("capped {sha}")))?;
            deps.store.flag_for_review(lane.id)?;
            actions.push(CiFixAction::Capped {
                ticket: ticket.to_string(),
                sha,
            });
            continue;
        }

        // Recorded before the launch, so the next tick sees this head as
        // handled even while the launched pass still waits on admission.
        deps.store
            .add_event(lane.id, CI_FIX_EVENT, Some(&format!("started {sha}")))?;
        match deps.launcher.launch(ticket) {
            Ok(()) => actions.push(CiFixAction::Started {
                ticket: ticket.to_string(),
                sha,
            }),
            Err(error) => {
                deps.store.add_event(
                    lane.id,
                    CI_FIX_EVENT,
                    Some(&format!("failed {sha}: {error}")),
                )?;
                actions.push(CiFixAction::LaunchFailed {
                    ticket: ticket.to_string(),
                    sha,
                    error,
                });
            }
        }
    }
    Ok(actions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::gh_cli::{CheckResult, FakeGhCli, GhError};
    use crate::runs::{FinishRun, StartRun};
    use crate::work::fix_cap::REVIEW_FIX_KIND;
    use std::cell::RefCell;
    use tempfile::TempDir;

    const SCOPE: &str = "gh:o/r";

    fn check(name: &str, state: ChecksState, run_id: Option<u64>) -> CheckResult {
        CheckResult {
            name: name.to_string(),
            state,
            run_id,
            job_id: None,
            details_url: run_id.map(|id| format!("https://github.com/o/r/actions/runs/{id}")),
            conclusion: match state {
                ChecksState::Failing => Some("FAILURE".to_string()),
                ChecksState::Passing => Some("SUCCESS".to_string()),
                ChecksState::Pending | ChecksState::None => None,
            },
        }
    }

    fn concluded(name: &str, conclusion: &str, run_id: u64) -> CheckResult {
        CheckResult {
            conclusion: Some(conclusion.to_string()),
            ..check(name, ChecksState::Failing, Some(run_id))
        }
    }

    fn pr_checks(sha: &str, checks: Vec<CheckResult>) -> PrChecks {
        PrChecks {
            lifecycle: PrLifecycle::Open,
            head_sha: sha.to_string(),
            checks,
        }
    }

    fn red(sha: &str) -> PrChecks {
        pr_checks(
            sha,
            vec![
                check("lint", ChecksState::Passing, Some(1)),
                check("test", ChecksState::Failing, Some(2)),
            ],
        )
    }

    fn event(kind: &str, detail: &str) -> RunEvent {
        RunEvent {
            id: 1,
            at: "2026-11-04T00:00:00Z".to_string(),
            kind: kind.to_string(),
            detail: Some(detail.to_string()),
        }
    }

    // --- ready_to_fix ---

    #[test]
    fn ready_when_a_check_failed_and_none_is_pending() {
        assert!(ready_to_fix(&red("abc")));
    }

    #[test]
    fn not_ready_while_a_check_is_pending() {
        let checks = pr_checks(
            "abc",
            vec![
                check("test", ChecksState::Failing, Some(2)),
                check("build", ChecksState::Pending, Some(3)),
            ],
        );
        assert!(!ready_to_fix(&checks));
    }

    #[test]
    fn not_ready_when_everything_passes_or_there_are_no_checks() {
        let passing = pr_checks("abc", vec![check("test", ChecksState::Passing, Some(2))]);
        assert!(!ready_to_fix(&passing));
        assert!(!ready_to_fix(&pr_checks("abc", Vec::new())));
    }

    #[test]
    fn not_ready_when_the_only_red_checks_were_cancelled_or_await_approval() {
        let checks = pr_checks(
            "abc",
            vec![
                check("lint", ChecksState::Passing, Some(1)),
                concluded("test", "CANCELLED", 2),
                concluded("deploy", "ACTION_REQUIRED", 3),
            ],
        );
        assert!(!ready_to_fix(&checks));
    }

    #[test]
    fn ready_when_a_real_failure_sits_beside_a_cancelled_check() {
        let checks = pr_checks(
            "abc",
            vec![
                concluded("test", "CANCELLED", 2),
                concluded("build", "TIMED_OUT", 3),
            ],
        );
        assert!(ready_to_fix(&checks));
    }

    // --- handled ---

    #[test]
    fn handled_matches_any_outcome_for_the_same_sha() {
        for detail in ["started abc", "capped abc", "failed abc: no tmux"] {
            assert!(handled(&[event(CI_FIX_EVENT, detail)], "abc"), "{detail}");
        }
    }

    #[test]
    fn handled_ignores_other_shas_and_other_event_kinds() {
        let events = [
            event(CI_FIX_EVENT, "started abc"),
            event("note", "started def"),
        ];
        assert!(!handled(&events, "def"));
        assert!(!handled(&events, "ab"));
        assert!(!handled(&[], "abc"));
    }

    // --- trim_log ---

    #[test]
    fn trim_log_leaves_a_short_log_unchanged() {
        assert_eq!(trim_log("one\ntwo\n", 100), "one\ntwo\n");
    }

    #[test]
    fn trim_log_keeps_the_tail_from_a_line_start_with_a_marker() {
        let log = "first line\nsecond line\nerror: boom\n";
        let trimmed = trim_log(log, 20);
        assert!(trimmed.ends_with("error: boom\n"), "{trimmed}");
        assert!(!trimmed.contains("second"), "{trimmed}");
        let kept = "error: boom\n".len();
        assert!(
            trimmed.starts_with(&format!("[... {} bytes trimmed ...]\n", log.len() - kept)),
            "{trimmed}"
        );
    }

    #[test]
    fn trim_log_cuts_on_a_char_boundary() {
        // No newline to align to, and the byte cut lands inside a 3-byte char.
        let log = "€€€€€";
        let trimmed = trim_log(log, 4);
        assert!(trimmed.ends_with('€'), "{trimmed}");
        assert!(trimmed.contains("bytes trimmed"), "{trimmed}");
        assert!(!trimmed.ends_with("€€"), "{trimmed}");
    }

    // --- build_prompt ---

    #[test]
    fn build_prompt_names_the_pr_the_checks_and_their_logs() {
        let failed = [
            FailedCheck {
                name: "test".to_string(),
                details_url: Some("https://github.com/o/r/actions/runs/2".to_string()),
                log: Ok("assertion failed: left == right".to_string()),
            },
            FailedCheck {
                name: "ci/external".to_string(),
                details_url: None,
                log: Err("not a GitHub Actions run".to_string()),
            },
        ];
        let prompt = build_prompt("https://github.com/o/r/pull/7", "abc123", &failed);

        assert!(prompt.contains("https://github.com/o/r/pull/7"), "{prompt}");
        assert!(prompt.contains("abc123"), "{prompt}");
        assert!(
            prompt.contains("- test (https://github.com/o/r/actions/runs/2)"),
            "{prompt}"
        );
        assert!(prompt.contains("- ci/external\n"), "{prompt}");
        assert!(
            prompt.contains("## test\n\n```\nassertion failed: left == right\n```"),
            "{prompt}"
        );
        assert!(
            prompt.contains("## ci/external\n\nCould not fetch the log: not a GitHub Actions run"),
            "{prompt}"
        );
        assert!(
            prompt.contains("do not create a new branch or worktree"),
            "{prompt}"
        );
        assert!(prompt.contains("push"), "{prompt}");
    }

    #[test]
    fn build_prompt_splits_the_log_budget_across_checks() {
        let big = "x\n".repeat(LOG_LIMIT_BYTES);
        let failed: Vec<FailedCheck> = ["a", "b"]
            .iter()
            .map(|name| FailedCheck {
                name: (*name).to_string(),
                details_url: None,
                log: Ok(big.clone()),
            })
            .collect();
        let prompt = build_prompt("https://github.com/o/r/pull/7", "abc", &failed);

        assert!(prompt.len() < LOG_LIMIT_BYTES + 2_000, "{}", prompt.len());
        assert_eq!(prompt.matches("bytes trimmed").count(), 2);
    }

    // --- collect_failures ---

    #[test]
    fn collect_failures_fetches_each_shared_run_once() {
        let gh = FakeGhCli::new()
            .with_run_failed_log(2, Ok("run 2 log".to_string()))
            .with_run_failed_log(
                3,
                Err(GhError::Command {
                    command: "gh run view".to_string(),
                    exit_code: Some(1),
                    stderr: "no such run".to_string(),
                }),
            );
        let checks = pr_checks(
            "abc",
            vec![
                check("unit", ChecksState::Failing, Some(2)),
                check("lint", ChecksState::Passing, Some(2)),
                check("integration", ChecksState::Failing, Some(2)),
                check("e2e", ChecksState::Failing, Some(3)),
                CheckResult {
                    name: "ci/external".to_string(),
                    state: ChecksState::Failing,
                    run_id: None,
                    job_id: None,
                    details_url: Some("https://ci.example.com/9".to_string()),
                    conclusion: Some("FAILURE".to_string()),
                },
                concluded("cancelled", "CANCELLED", 4),
            ],
        );

        let failed = collect_failures(&gh, Path::new("/repo"), &checks);

        assert_eq!(gh.run_failed_log_calls(), vec![2, 3]);
        let names: Vec<&str> = failed.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["unit", "integration", "e2e", "ci/external"]);
        assert_eq!(failed[0].log, Ok("run 2 log".to_string()));
        assert_eq!(failed[1].log, Ok("run 2 log".to_string()));
        assert!(failed[2].log.as_ref().unwrap_err().contains("no such run"));
        let external = failed[3].log.as_ref().unwrap_err();
        assert!(external.contains("https://ci.example.com/9"), "{external}");
    }

    #[test]
    fn collect_failures_fetches_each_job_alone_when_the_link_names_it() {
        let gh = FakeGhCli::new()
            .with_job_failed_log(21, Ok("unit log".to_string()))
            .with_job_failed_log(22, Ok("integration log".to_string()));
        let job = |name: &str, job_id: u64| CheckResult {
            job_id: Some(job_id),
            details_url: Some(format!(
                "https://github.com/o/r/actions/runs/2/job/{job_id}"
            )),
            ..check(name, ChecksState::Failing, Some(2))
        };
        let checks = pr_checks("abc", vec![job("unit", 21), job("integration", 22)]);

        let failed = collect_failures(&gh, Path::new("/repo"), &checks);

        assert_eq!(gh.job_failed_log_calls(), vec![21, 22]);
        assert!(gh.run_failed_log_calls().is_empty());
        assert_eq!(failed[0].log, Ok("unit log".to_string()));
        assert_eq!(failed[1].log, Ok("integration log".to_string()));
    }

    // --- sweep ---

    struct FakeLauncher {
        launched: RefCell<Vec<String>>,
        error: Option<String>,
    }

    impl FakeLauncher {
        fn new() -> Self {
            Self {
                launched: RefCell::new(Vec::new()),
                error: None,
            }
        }

        fn failing(error: &str) -> Self {
            Self {
                error: Some(error.to_string()),
                ..Self::new()
            }
        }

        fn launched(&self) -> Vec<String> {
            self.launched.borrow().clone()
        }
    }

    impl CiFixLauncher for FakeLauncher {
        fn launch(&self, ticket: &str) -> Result<(), String> {
            self.launched.borrow_mut().push(ticket.to_string());
            match &self.error {
                Some(error) => Err(error.clone()),
                None => Ok(()),
            }
        }
    }

    struct Fixture {
        _tmp: TempDir,
        store: RunStore,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = TempDir::new().unwrap();
            let store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
            Self { _tmp: tmp, store }
        }

        fn start(&self, scope: &str, ticket: &str, kind: &str) -> i64 {
            self.store
                .start_run(&StartRun {
                    ticket: ticket.to_string(),
                    scope: scope.to_string(),
                    lane: "backend".to_string(),
                    worktree: format!("/wt/{ticket}"),
                    branch: None,
                    pid: None,
                    kind: kind.to_string(),
                    log_path: None,
                })
                .unwrap()
        }

        fn finish(&self, run_id: i64, pr_url: Option<&str>) {
            self.store
                .finish_run(
                    run_id,
                    &FinishRun {
                        status: RunStatus::Done,
                        pr_url: pr_url.map(str::to_string),
                        ..FinishRun::default()
                    },
                )
                .unwrap();
        }

        /// A finished lane run for `ticket` in `scope` with PR `number`.
        fn lane(&self, scope: &str, ticket: &str, number: u64) -> i64 {
            let id = self.start(scope, ticket, "lane");
            self.finish(id, Some(&format!("https://github.com/o/r/pull/{number}")));
            id
        }

        fn sweep(
            &self,
            gh: &FakeGhCli,
            launcher: &FakeLauncher,
            max_fix_rounds: u32,
        ) -> Vec<CiFixAction> {
            let runs = self.store.overview_runs().unwrap();
            let deps = SweepDeps {
                store: &self.store,
                gh,
                repo_dir: Path::new("/repo"),
                launcher,
            };
            sweep(&deps, &runs, SCOPE, max_fix_rounds).unwrap()
        }

        fn ci_fix_details(&self, run_id: i64) -> Vec<String> {
            self.store
                .events_for_run(run_id)
                .unwrap()
                .into_iter()
                .filter(|e| e.kind == CI_FIX_EVENT)
                .filter_map(|e| e.detail)
                .collect()
        }

        fn status(&self, run_id: i64) -> RunStatus {
            self.store
                .overview_runs()
                .unwrap()
                .into_iter()
                .find(|run| run.id == run_id)
                .unwrap()
                .status
        }
    }

    fn started(ticket: &str, sha: &str) -> CiFixAction {
        CiFixAction::Started {
            ticket: ticket.to_string(),
            sha: sha.to_string(),
        }
    }

    #[test]
    fn a_completed_failing_run_starts_one_fix_and_records_it() {
        let fx = Fixture::new();
        let lane = fx.lane(SCOPE, "GH-1", 7);
        let gh = FakeGhCli::new().with_pr_checks(7, Ok(red("abc")));
        let launcher = FakeLauncher::new();

        let actions = fx.sweep(&gh, &launcher, 3);

        assert_eq!(actions, vec![started("GH-1", "abc")]);
        assert_eq!(launcher.launched(), vec!["GH-1"]);
        assert_eq!(gh.pr_checks_calls(), vec![7]);
        assert_eq!(fx.ci_fix_details(lane), vec!["started abc"]);
    }

    #[test]
    fn a_second_sweep_on_the_same_head_does_nothing() {
        let fx = Fixture::new();
        fx.lane(SCOPE, "GH-1", 7);
        let gh = FakeGhCli::new().with_pr_checks(7, Ok(red("abc")));
        let launcher = FakeLauncher::new();

        fx.sweep(&gh, &launcher, 3);
        let actions = fx.sweep(&gh, &launcher, 3);

        assert!(actions.is_empty(), "{actions:?}");
        assert_eq!(launcher.launched(), vec!["GH-1"]);
    }

    #[test]
    fn a_new_head_failing_again_starts_a_retry() {
        let fx = Fixture::new();
        let lane = fx.lane(SCOPE, "GH-1", 7);
        let launcher = FakeLauncher::new();
        fx.sweep(
            &FakeGhCli::new().with_pr_checks(7, Ok(red("abc"))),
            &launcher,
            3,
        );

        let actions = fx.sweep(
            &FakeGhCli::new().with_pr_checks(7, Ok(red("def"))),
            &launcher,
            3,
        );

        assert_eq!(actions, vec![started("GH-1", "def")]);
        assert_eq!(launcher.launched(), vec!["GH-1", "GH-1"]);
        assert_eq!(fx.ci_fix_details(lane), vec!["started abc", "started def"]);
    }

    #[test]
    fn pending_or_passing_checks_do_not_trigger() {
        let fx = Fixture::new();
        fx.lane(SCOPE, "GH-1", 7);
        fx.lane(SCOPE, "GH-2", 8);
        let pending = pr_checks(
            "abc",
            vec![
                check("test", ChecksState::Failing, Some(2)),
                check("build", ChecksState::Pending, Some(3)),
            ],
        );
        let passing = pr_checks("def", vec![check("test", ChecksState::Passing, Some(4))]);
        let gh = FakeGhCli::new()
            .with_pr_checks(7, Ok(pending))
            .with_pr_checks(8, Ok(passing));
        let launcher = FakeLauncher::new();

        let actions = fx.sweep(&gh, &launcher, 3);

        assert!(actions.is_empty(), "{actions:?}");
        assert!(launcher.launched().is_empty());
    }

    #[test]
    fn at_the_cap_flags_the_lane_for_review_once_instead_of_launching() {
        let fx = Fixture::new();
        let lane = fx.lane(SCOPE, "GH-1", 7);
        let fix = fx.start(SCOPE, "GH-1", REVIEW_FIX_KIND);
        fx.finish(fix, None);
        let gh = FakeGhCli::new().with_pr_checks(7, Ok(red("abc")));
        let launcher = FakeLauncher::new();

        let actions = fx.sweep(&gh, &launcher, 1);

        assert_eq!(
            actions,
            vec![CiFixAction::Capped {
                ticket: "GH-1".to_string(),
                sha: "abc".to_string(),
            }]
        );
        assert!(launcher.launched().is_empty());
        assert_eq!(fx.ci_fix_details(lane), vec!["capped abc"]);
        assert_eq!(fx.status(lane), RunStatus::Review);

        let again = fx.sweep(&gh, &launcher, 1);
        assert!(again.is_empty(), "{again:?}");
        assert_eq!(fx.ci_fix_details(lane), vec!["capped abc"]);
    }

    #[test]
    fn a_live_run_for_the_ticket_skips_it() {
        let fx = Fixture::new();
        fx.lane(SCOPE, "GH-1", 7);
        fx.start(SCOPE, "GH-1", REVIEW_FIX_KIND);
        let gh = FakeGhCli::new().with_pr_checks(7, Ok(red("abc")));
        let launcher = FakeLauncher::new();

        let actions = fx.sweep(&gh, &launcher, 3);

        assert!(actions.is_empty(), "{actions:?}");
        assert!(gh.pr_checks_calls().is_empty());
    }

    #[test]
    fn a_closed_pr_skips_the_ticket() {
        let fx = Fixture::new();
        let lane = fx.lane(SCOPE, "GH-1", 7);
        fx.store
            .add_event(lane, "pr_closed", Some(r#"{"reason":"merged"}"#))
            .unwrap();
        let gh = FakeGhCli::new().with_pr_checks(7, Ok(red("abc")));
        let launcher = FakeLauncher::new();

        assert!(fx.sweep(&gh, &launcher, 3).is_empty());
        assert!(gh.pr_checks_calls().is_empty());
    }

    #[test]
    fn a_pr_github_reports_merged_skips_the_ticket() {
        let fx = Fixture::new();
        fx.lane(SCOPE, "GH-1", 7);
        let merged = PrChecks {
            lifecycle: PrLifecycle::Merged,
            ..red("abc")
        };
        let gh = FakeGhCli::new().with_pr_checks(7, Ok(merged));
        let launcher = FakeLauncher::new();

        assert!(fx.sweep(&gh, &launcher, 3).is_empty());
        assert!(launcher.launched().is_empty());
    }

    #[test]
    fn a_pr_github_reports_merged_or_closed_is_recorded_and_not_polled_again() {
        let fx = Fixture::new();
        let merged_lane = fx.lane(SCOPE, "GH-1", 7);
        let closed_lane = fx.lane(SCOPE, "GH-2", 8);
        let gh = FakeGhCli::new()
            .with_pr_checks(
                7,
                Ok(PrChecks {
                    lifecycle: PrLifecycle::Merged,
                    ..red("abc")
                }),
            )
            .with_pr_checks(
                8,
                Ok(PrChecks {
                    lifecycle: PrLifecycle::Closed,
                    ..red("def")
                }),
            );
        let launcher = FakeLauncher::new();

        assert!(fx.sweep(&gh, &launcher, 3).is_empty());
        assert_eq!(gh.pr_checks_calls(), vec![8, 7]);
        let closed_detail = |run_id| {
            fx.store
                .events_for_run(run_id)
                .unwrap()
                .into_iter()
                .filter(|e| e.kind == "pr_closed")
                .filter_map(|e| e.detail)
                .collect::<Vec<_>>()
        };
        assert_eq!(closed_detail(merged_lane), vec![r#"{"reason":"merged"}"#]);
        assert_eq!(closed_detail(closed_lane), vec![r#"{"reason":"closed"}"#]);

        assert!(fx.sweep(&gh, &launcher, 3).is_empty());
        assert_eq!(gh.pr_checks_calls(), vec![8, 7]);
    }

    #[test]
    fn a_ticket_without_a_lane_pr_or_in_another_scope_is_ignored() {
        let fx = Fixture::new();
        let no_pr = fx.start(SCOPE, "GH-1", "lane");
        fx.finish(no_pr, None);
        fx.lane("gh:x/y", "GH-2", 7);
        let gh = FakeGhCli::new().with_pr_checks(7, Ok(red("abc")));
        let launcher = FakeLauncher::new();

        assert!(fx.sweep(&gh, &launcher, 3).is_empty());
        assert!(gh.pr_checks_calls().is_empty());
    }

    #[test]
    fn a_gh_error_is_reported_and_other_tickets_still_run() {
        let fx = Fixture::new();
        fx.lane(SCOPE, "GH-1", 7);
        fx.lane(SCOPE, "GH-2", 8);
        let gh = FakeGhCli::new()
            .with_pr_checks(
                8,
                Err(GhError::Command {
                    command: "gh pr view".to_string(),
                    exit_code: Some(1),
                    stderr: "rate limited".to_string(),
                }),
            )
            .with_pr_checks(7, Ok(red("abc")));
        let launcher = FakeLauncher::new();

        let actions = fx.sweep(&gh, &launcher, 3);

        assert_eq!(actions.len(), 2, "{actions:?}");
        assert!(matches!(
            &actions[0],
            CiFixAction::CheckFailed { ticket, error }
                if ticket == "GH-2" && error.contains("rate limited")
        ));
        assert_eq!(actions[1], started("GH-1", "abc"));
    }

    #[test]
    fn a_launch_error_is_recorded_and_not_retried_for_the_head() {
        let fx = Fixture::new();
        let lane = fx.lane(SCOPE, "GH-1", 7);
        let gh = FakeGhCli::new().with_pr_checks(7, Ok(red("abc")));
        let launcher = FakeLauncher::failing("no tmux");

        let actions = fx.sweep(&gh, &launcher, 3);

        assert_eq!(
            actions,
            vec![CiFixAction::LaunchFailed {
                ticket: "GH-1".to_string(),
                sha: "abc".to_string(),
                error: "no tmux".to_string(),
            }]
        );
        assert_eq!(
            fx.ci_fix_details(lane),
            vec!["started abc", "failed abc: no tmux"]
        );
        assert!(fx.sweep(&gh, &launcher, 3).is_empty());
    }
}
