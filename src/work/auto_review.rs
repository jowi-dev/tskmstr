//! `tm review auto <KEY>` (GitHub issue #124): review rounds that write
//! thatch findings into vdiff's comment store and drive `tm review fix`
//! until the review is clean or a round cap is hit. See
//! `docs/plans/gh-124-auto-review.md`.
//!
//! [`run_loop`] is the whole loop. Everything that runs an agent, posts to
//! GitHub or touches the run store goes through [`AutoReviewOps`], so the
//! loop itself is tested without any of them.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

use crate::config::{AutoReviewConfig, Severity};
use crate::work::comment_store::{self, CommentStoreError, NewComment};

/// Run `kind` of each review round's agent run.
pub const AUTO_REVIEW_KIND: &str = "auto-review";

/// Event kind recorded on each round's run row. Its detail is
/// `<round>/<max> <verdict>`; see [`RoundVerdict`].
pub const ROUND_EVENT: &str = "auto_review_round";

/// Event kind the loop records on every review and fix row it creates,
/// its detail the loop's own pid (GitHub issue #150). An interactive row's
/// pid is its agent's, which outlives the loop in its open window, so the
/// board's `review:` badge reads the loop's liveness from this event.
pub const LOOP_PID_EVENT: &str = "auto_review_loop_pid";

/// Event kind the loop records on a review or fix row once its step's
/// signal file is read, its detail `review` or `fix` (GitHub issue #150).
/// The row itself stays `running` until its agent exits, so the idle sweep
/// hibernates the agent and admission counts it; this event is what marks
/// the window as finished for [`crate::work::hibernate::ended_launch_windows`].
pub const SIGNAL_EVENT: &str = "auto_review_signalled";

/// Skill a first round reviews with.
pub const FIRST_ROUND_SKILL: &str = "thatch-code-review";

/// Skill every later round reviews with, against the previous round's
/// archived comments.
pub const FOLLOWUP_SKILL: &str = "thatch-review-followup";

/// Built-in round prompt, used when `[work.auto_review]` sets neither
/// `prompt` nor `prompt_file`. Placeholders are those of [`render_prompt`].
pub const DEFAULT_PROMPT: &str = "\
Review the changes on this branch for {key} (auto-review round {round}) \
with the `{skill}` skill. Do not post anything to GitHub and do not change \
any code.

{prior}Report every CONFIRMED finding as one object in a JSON array:

    {\"path\": \"src/lib.rs\", \"start_line\": 10, \"end_line\": 12,
     \"severity\": \"MEDIUM\", \"specialist\": \"state-flow\",
     \"body\": \"What is wrong, why, and what to change.\"}

`path` is repo-relative; lines are 1-based and inclusive, anchored to the \
head SHA. `severity` is one of LOW, MEDIUM, HIGH, BLOCKING. The completion \
signal section below says where and how to write the array.
";

/// Errors from [`run_loop`].
#[derive(Debug, Error)]
pub enum AutoReviewError {
    /// Reading or writing the comment store failed.
    #[error(transparent)]
    Store(#[from] CommentStoreError),

    /// A step's signal file (a round's findings file or a fix pass's
    /// marker) was unreadable, or complete JSON of the wrong shape.
    #[error("auto-review signal file {path} is unusable: {message}")]
    Findings {
        /// The signal file.
        path: PathBuf,
        /// What was wrong with it.
        message: String,
    },

    /// One of [`AutoReviewOps`]' steps failed.
    #[error("auto-review {step} failed: {message}")]
    Step {
        /// Which step: `review`, `import`, `fix`, `publish` or `record`.
        step: &'static str,
        /// The underlying error, rendered.
        message: String,
    },
}

impl AutoReviewError {
    /// Wraps any displayable error as a [`AutoReviewError::Step`].
    pub fn step(step: &'static str, err: impl std::fmt::Display) -> Self {
        AutoReviewError::Step {
            step,
            message: err.to_string(),
        }
    }
}

fn default_specialist() -> String {
    "review".to_string()
}

/// One confirmed finding, as a review round writes it to its findings file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Finding {
    /// Repo-relative path.
    pub path: String,
    /// 1-based, inclusive first line.
    pub start_line: u64,
    /// 1-based, inclusive last line; `start_line` when absent.
    #[serde(default)]
    pub end_line: Option<u64>,
    /// Severity name; see [`Finding::severity`].
    pub severity: String,
    /// The specialist lens that found it, e.g. `state-flow`.
    #[serde(default = "default_specialist")]
    pub specialist: String,
    /// The finding itself.
    pub body: String,
}

impl Finding {
    /// The parsed severity. An unknown name counts as [`Severity::Low`], so
    /// a malformed finding is still shown but never forces a fix.
    pub fn severity(&self) -> Severity {
        Severity::parse(&self.severity).unwrap_or(Severity::Low)
    }

    /// The comment-store entry for this finding, its text prefixed with
    /// provenance and severity so it reads apart from a human's comment.
    pub fn to_comment(&self) -> NewComment {
        NewComment {
            path: self.path.clone(),
            start_line: self.start_line,
            end_line: self
                .end_line
                .unwrap_or(self.start_line)
                .max(self.start_line),
            text: format!(
                "[auto-review · {} · {}] {}",
                self.specialist,
                self.severity().as_str(),
                self.body
            ),
        }
    }
}

/// Reads signal file `path` if it is ready: `Ok(None)` while it is missing
/// or ends before its JSON does, so a half-written file reads as "not done"
/// (GitHub issue #150).
///
/// # Errors
///
/// [`AutoReviewError::Findings`] for a file that cannot be read, or whose
/// JSON is complete but not a `T`: waiting longer would never fix it.
fn read_signal<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, AutoReviewError> {
    let unusable = |message: String| AutoReviewError::Findings {
        path: path.to_path_buf(),
        message,
    };
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(unusable(e.to_string())),
    };
    match serde_json::from_str(&raw) {
        Ok(signal) => Ok(Some(signal)),
        Err(e) if e.is_eof() => Ok(None),
        Err(e) => Err(unusable(e.to_string())),
    }
}

/// Where round `round` of loop `loop_id` writes its findings:
/// `<git_dir>/vdiff/auto-review/findings-<loop_id>-<round>.json`. Under the
/// git dir, like the comment store, so it never shows in `git status`.
///
/// `loop_id` (see [`run_loop`]) keeps each loop's signal files apart, so an
/// agent left behind by an earlier loop, still working after its step timed
/// out, can never write a file a later loop reads as its own step's signal.
pub fn findings_path(git_dir: &Path, loop_id: &str, round: u32) -> PathBuf {
    git_dir
        .join("vdiff")
        .join("auto-review")
        .join(format!("findings-{loop_id}-{round}.json"))
}

/// Reads a round's findings file if it is ready; see [`read_signal`].
///
/// # Errors
///
/// [`AutoReviewError::Findings`] if the file is complete but not a JSON
/// array of [`Finding`]s.
pub fn try_read_findings(path: &Path) -> Result<Option<Vec<Finding>>, AutoReviewError> {
    read_signal(path)
}

/// Where round `round`'s fix pass in loop `loop_id` writes its completion
/// marker: `<git_dir>/vdiff/auto-review/fix-<loop_id>-<round>.done`, next to
/// the findings files and keyed the same way (GitHub issue #150). It holds
/// `{"succeeded": true}` or `{"succeeded": false}`, plus the comments the
/// pass declined (GitHub issue #204); see [`read_fix_marker`].
pub fn fix_marker_path(git_dir: &Path, loop_id: &str, round: u32) -> PathBuf {
    git_dir
        .join("vdiff")
        .join("auto-review")
        .join(format!("fix-{loop_id}-{round}.done"))
}

/// A fix pass's completion marker, as [`fix_completion_section`] asks the
/// agent to write it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct FixMarker {
    /// Whether the pass fixed what it set out to fix.
    pub succeeded: bool,
    /// The comments the pass triaged as not needing a change (GitHub issue
    /// #204). Absent from an older agent's marker, so it defaults to empty.
    #[serde(default)]
    pub declined: Vec<Decline>,
}

/// One review comment a fix pass declined, which [`run_loop`] hands the
/// next round so `thatch-review-followup` checks the reason instead of
/// re-raising the finding as unaddressed.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Decline {
    /// The comment's `path:start-end` anchor.
    pub comment: String,
    /// Why no change was made, in one line.
    pub reason: String,
}

/// A fix pass's marker: `Some` once [`fix_marker_path`]'s file exists and
/// parses, `None` before then; see [`read_signal`].
///
/// # Errors
///
/// [`AutoReviewError::Findings`] if the file is complete JSON but not a
/// marker.
pub fn read_fix_marker(path: &Path) -> Result<Option<FixMarker>, AutoReviewError> {
    read_signal(path)
}

/// The temporary name an agent writes a signal file under before renaming
/// it into place.
fn temporary_name(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    PathBuf::from(name)
}

/// The section tm appends to every review round's prompt, whatever the
/// template says: the loop waits for `findings_file`, so the agent writes
/// it last, under a temporary name, and renames it into place.
pub fn review_completion_section(findings_file: &Path) -> String {
    let tmp = temporary_name(findings_file);
    format!(
        "\n\n## Completion signal\n\n\
         tm is waiting for `{file}` to know this round is done. Once the \
         review is complete, write the findings JSON array (`[]` when there \
         are none) to `{tmp}` first, then rename it into place with `mv \
         '{tmp}' '{file}'`, so tm never reads a half-written file. Write it \
         even if the review is clean. Then stop and leave this session open.\n",
        file = findings_file.display(),
        tmp = tmp.display(),
    )
}

/// The section tm appends to an auto-review fix pass's prompt: the loop
/// waits for `marker` ([`fix_marker_path`]), written last under a
/// temporary name and renamed into place, carrying every comment the pass
/// made no code change for, merged ones included ([`FixMarker`]).
pub fn fix_completion_section(marker: &Path) -> String {
    let tmp = temporary_name(marker);
    format!(
        "\n\n## Completion signal\n\n\
         tm is waiting for `{file}` to know this fix pass is done. Once every \
         comment is triaged and every fix committed, write \
         `{{\"succeeded\": true}}` to `{tmp}`, or `{{\"succeeded\": false}}` \
         if you could not fix the legitimate ones. List every comment that \
         got no code change under `declined` in the same object, so the next \
         review round checks your reason instead of re-raising it. That \
         includes a comment you merged with another: give it a reason like \
         `same root cause as <anchor>: ...`.\n\n\
         \x20   {{\"succeeded\": true, \"declined\": [{{\"comment\": \"src/lib.rs:10-12\",\n\
         \x20     \"reason\": \"One line on why no change is needed.\"}}]}}\n\n\
         Then rename it into place with `mv '{tmp}' '{file}'`, so tm never \
         reads a half-written file. Then stop and leave this session open.\n",
        file = marker.display(),
        tmp = tmp.display(),
    )
}

/// Renders a round's prompt from `template`, replacing `{key}`, `{round}`,
/// `{skill}`, `{findings_file}`, `{archive}` (empty on round 1) and
/// `{prior}` (empty on round 1: a sentence pointing a follow-up round at
/// `archive`, then the previous fix pass's `declined` comments with their
/// reasons), then appends [`review_completion_section`], so a custom
/// template that never mentions the findings file still signals.
pub fn render_prompt(
    template: &str,
    key: &str,
    round: u32,
    findings_file: &Path,
    archive: Option<&Path>,
    declined: &[Decline],
) -> String {
    let skill = if round <= 1 {
        FIRST_ROUND_SKILL
    } else {
        FOLLOWUP_SKILL
    };
    let archive_text = archive
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let prior = match archive {
        Some(path) => format!(
            "Your prior round's findings are in `{}` (vdiff comment-store \
             JSON; yours are the entries whose `text` starts with \
             `[auto-review · ...]`, any others are a human's). They were never \
             posted to GitHub: use this file, not the PR, as your prior review \
             comments, and verify whether each was addressed since.\n\n{}",
            path.display(),
            declined_section(declined),
        ),
        None => String::new(),
    };
    template
        .replace("{key}", key)
        .replace("{round}", &round.to_string())
        .replace("{skill}", skill)
        .replace("{findings_file}", &findings_file.to_string_lossy())
        .replace("{archive}", &archive_text)
        .replace("{prior}", &prior)
        + &review_completion_section(findings_file)
}

/// The part of a follow-up round's `{prior}` listing what the previous fix
/// pass declined, empty when it declined nothing.
fn declined_section(declined: &[Decline]) -> String {
    if declined.is_empty() {
        return String::new();
    }
    let mut section = String::from(
        "The fix pass declined these comments without changing code. Check \
         each reason: re-report a finding only if its reason does not hold, \
         and do not count it as unaddressed otherwise.\n\n",
    );
    for decline in declined {
        section.push_str(&format!("- `{}`: {}\n", decline.comment, decline.reason));
    }
    section.push('\n');
    section
}

/// Seconds between two checks for a step's signal file.
pub const SIGNAL_POLL_SECS: u64 = 10;

/// A step's run row, as the loop sees it while waiting for the step's
/// signal file (GitHub issue #150).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    /// Still running: keep waiting.
    Running,
    /// The idle sweep hibernated the agent before it signalled: the step is
    /// stuck.
    Hibernated,
    /// Something other than the loop ended the row (`tm merge` finishing
    /// the ticket's runs, the user exiting the session), or the ticket's
    /// session no longer exists: the loop has nothing left to drive.
    Gone,
}

/// Everything [`run_loop`] needs done outside the comment store.
///
/// A review round and a fix pass are each launched, not run to completion:
/// [`run_loop`] then polls the step's signal file (the round's findings
/// file, the fix pass's [`fix_marker_path`] marker), checking
/// [`AutoReviewOps::step_state`] between polls.
pub trait AutoReviewOps {
    /// Launches one review round's agent with `prompt` and returns its run
    /// row id. The agent writes its findings to the `findings_file` the
    /// prompt names.
    fn review(&self, round: u32, prompt: String) -> Result<i64, AutoReviewError>;
    /// Launches `tm review fix` over the current store for round `round`,
    /// with `completion` ([`fix_completion_section`]) appended to its
    /// prompt, and returns its run row id. `None` when the store had nothing
    /// to fix, so there is nothing to wait for.
    fn fix(&self, round: u32, completion: String) -> Result<Option<i64>, AutoReviewError>;
    /// The state of a launched step's run row `run_id`.
    fn step_state(&self, run_id: i64) -> Result<StepState, AutoReviewError>;
    /// Records that run `run_id`'s `step` (`review` or `fix`) signalled, as
    /// a [`SIGNAL_EVENT`]. The row keeps its status: its agent is still
    /// open in its window.
    fn mark_signalled(&self, run_id: i64, step: &'static str) -> Result<(), AutoReviewError>;
    /// Waits `secs` seconds between two signal polls.
    fn sleep(&self, secs: u64);
    /// Posts the store with `vdiff --publish-comments <pr>`.
    fn publish(&self) -> Result<(), AutoReviewError>;
    /// Records an event on run `run_id`.
    fn record(&self, run_id: i64, kind: &str, detail: &str) -> Result<(), AutoReviewError>;
    /// Flags run `run_id` for a human: the loop hit its cap, a fix pass
    /// failed, or a step never signalled.
    fn flag(&self, run_id: i64) -> Result<(), AutoReviewError>;
    /// Whether the ticket has used up `[dispatch] max_fix_rounds`, the fix
    /// cap shared by every automatic trigger (GitHub issue #120).
    fn fix_cap_reached(&self) -> Result<bool, AutoReviewError>;
    /// Seconds since the Unix epoch, for comments' `created_at` and the
    /// signal timeout.
    fn now_unix_secs(&self) -> i64;
    /// Imports the PR's not-yet-imported, unresolved review-bot threads into
    /// the comment store, so the fix about to run addresses them too
    /// (GitHub issue #145). Returns how many were imported.
    fn import_bot_comments(&self) -> Result<usize, AutoReviewError>;
}

/// How waiting on one step's signal ended.
enum Signal<T> {
    /// The signal file was read.
    Received(T),
    /// No signal before the timeout, or the agent was hibernated first.
    Stuck,
    /// See [`StepState::Gone`].
    Gone,
}

/// Polls `read` every [`SIGNAL_POLL_SECS`] until it yields the step's
/// signal, the step's row stops running, or `timeout_secs` pass. The signal
/// is read first on every poll, so a step that signalled before being
/// hibernated or ended still counts as done. A signal file that can never
/// parse flags the step and returns `read`'s error.
fn wait_for_signal<T>(
    ops: &dyn AutoReviewOps,
    run_id: i64,
    timeout_secs: i64,
    read: impl Fn() -> Result<Option<T>, AutoReviewError>,
) -> Result<Signal<T>, AutoReviewError> {
    let start = ops.now_unix_secs();
    loop {
        match read() {
            Ok(Some(signal)) => return Ok(Signal::Received(signal)),
            Ok(None) => {}
            Err(err) => {
                ops.flag(run_id)?;
                return Err(err);
            }
        }
        match ops.step_state(run_id)? {
            StepState::Running => {}
            StepState::Hibernated => return Ok(Signal::Stuck),
            StepState::Gone => return Ok(Signal::Gone),
        }
        if ops.now_unix_secs() - start >= timeout_secs {
            return Ok(Signal::Stuck);
        }
        ops.sleep(SIGNAL_POLL_SECS);
    }
}

/// How one round ended, recorded as the tail of its [`ROUND_EVENT`]
/// detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundVerdict {
    /// Nothing at or above the threshold: the loop is done.
    Clean,
    /// Findings at or above the threshold went to a fix round.
    Fix,
    /// Findings at or above the threshold, but this was the last round.
    Capped,
    /// The fix round failed; the loop stopped.
    FixFailed,
    /// The round or its fix pass never signalled before
    /// `round_timeout_mins`, or was hibernated first; its run is flagged
    /// and the loop stopped (GitHub issue #150).
    Stuck,
    /// The step's row was ended by something else, or the ticket's session
    /// went away (`tm merge`); the loop stopped without flagging.
    Stopped,
}

impl RoundVerdict {
    /// The word recorded in the event detail.
    pub fn as_str(self) -> &'static str {
        match self {
            RoundVerdict::Clean => "clean",
            RoundVerdict::Fix => "fix",
            RoundVerdict::Capped => "capped",
            RoundVerdict::FixFailed => "fix-failed",
            RoundVerdict::Stuck => "stuck",
            RoundVerdict::Stopped => "stopped",
        }
    }
}

/// How [`run_loop`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopOutcome {
    /// Rounds reviewed.
    pub rounds: u32,
    /// The last round's verdict: never [`RoundVerdict::Fix`].
    pub verdict: RoundVerdict,
}

/// Runs review rounds until one is clean, the fix round fails, or
/// `cfg.max_review_rounds` is reached. See the plan doc for the round
/// sequence; `git_dir` is the worktree's resolved git directory, and
/// `loop_id` names this loop's signal files ([`findings_path`]), unique
/// per loop.
///
/// Each round and each fix pass is launched and then waited on through its
/// signal file, for at most `cfg.round_timeout_mins` each (`0`: no limit).
/// A step that
/// signals gets a [`SIGNAL_EVENT`]; one that times out or is hibernated
/// first is flagged and ends the loop as [`RoundVerdict::Stuck`]; one whose
/// row is ended by something else ends it as [`RoundVerdict::Stopped`]. The
/// loop never ends a row itself: each one stays in flight until its agent
/// exits, and a flag on it applies then ([`crate::runs::RunStore::flag_for_review`]).
///
/// # Errors
///
/// Any step's error stops the loop and is returned as is.
pub fn run_loop(
    ops: &dyn AutoReviewOps,
    cfg: &AutoReviewConfig,
    template: &str,
    key: &str,
    git_dir: &Path,
    loop_id: &str,
) -> Result<LoopOutcome, AutoReviewError> {
    let max = cfg.max_review_rounds.max(1);
    // `0` turns the timeout off, as `idle_hibernate_mins = 0` turns off the
    // idle sweep.
    let timeout_secs = match cfg.round_timeout_mins {
        0 => i64::MAX,
        mins => i64::try_from(mins.saturating_mul(60)).unwrap_or(i64::MAX),
    };
    let mut archive: Option<PathBuf> = None;
    let mut declined: Vec<Decline> = Vec::new();
    for round in 1..=max {
        let findings_file = findings_path(git_dir, loop_id, round);
        let marker = fix_marker_path(git_dir, loop_id, round);
        if let Some(dir) = findings_file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| AutoReviewError::step("review", e))?;
        }

        let prompt = render_prompt(
            template,
            key,
            round,
            &findings_file,
            archive.as_deref(),
            &declined,
        );
        let run_id = ops.review(round, prompt)?;
        let stopped = |verdict: RoundVerdict| -> Result<LoopOutcome, AutoReviewError> {
            ops.record(run_id, ROUND_EVENT, &round_detail(round, max, verdict))?;
            Ok(LoopOutcome {
                rounds: round,
                verdict,
            })
        };
        let findings = match wait_for_signal(ops, run_id, timeout_secs, || {
            try_read_findings(&findings_file)
        })? {
            Signal::Received(findings) => {
                ops.mark_signalled(run_id, "review")?;
                findings
            }
            Signal::Stuck => {
                ops.flag(run_id)?;
                return stopped(RoundVerdict::Stuck);
            }
            Signal::Gone => return stopped(RoundVerdict::Stopped),
        };
        let comments: Vec<NewComment> = findings.iter().map(Finding::to_comment).collect();
        if !comments.is_empty() {
            let created_at = comment_store::iso8601_utc(ops.now_unix_secs());
            comment_store::append(git_dir, &comments, &created_at)?;
        }
        if cfg.publish && !comments.is_empty() {
            ops.publish()?;
        }

        let needs_fix = findings.iter().any(|f| f.severity() >= cfg.fix_threshold);
        let verdict = match (needs_fix, round == max) {
            (false, _) => RoundVerdict::Clean,
            (true, true) => RoundVerdict::Capped,
            (true, false) if ops.fix_cap_reached()? => RoundVerdict::Capped,
            (true, false) => RoundVerdict::Fix,
        };
        if verdict != RoundVerdict::Fix {
            if verdict == RoundVerdict::Capped {
                ops.flag(run_id)?;
            }
            return stopped(verdict);
        }

        // Review bots' comments ride along with a fix but never start one:
        // only this round's own findings decided `verdict` above.
        ops.import_bot_comments()?;
        declined.clear();
        let fixed = match ops.fix(round, fix_completion_section(&marker))? {
            None => true,
            Some(fix_run) => {
                match wait_for_signal(ops, fix_run, timeout_secs, || read_fix_marker(&marker))? {
                    Signal::Received(marker) => {
                        ops.mark_signalled(fix_run, "fix")?;
                        if !marker.succeeded {
                            ops.flag(fix_run)?;
                        }
                        declined = marker.declined;
                        marker.succeeded
                    }
                    Signal::Stuck => {
                        ops.flag(fix_run)?;
                        return stopped(RoundVerdict::Stuck);
                    }
                    Signal::Gone => return stopped(RoundVerdict::Stopped),
                }
            }
        };
        if !fixed {
            return stopped(RoundVerdict::FixFailed);
        }
        ops.record(
            run_id,
            ROUND_EVENT,
            &round_detail(round, max, RoundVerdict::Fix),
        )?;
        archive = comment_store::archive(git_dir, round)?;
    }
    unreachable!("the last round always returns")
}

fn round_detail(round: u32, max: u32, verdict: RoundVerdict) -> String {
    format!("{round}/{max} {}", verdict.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use tempfile::TempDir;

    /// Scripted ops: round `n`'s agent "writes" `findings[n - 1]` to the
    /// findings file the prompt names, each fix pass writes its marker from
    /// `fix_results` (`true` once they run out), and every call is logged.
    /// A launched step writes its signal after `signal_after` sleeps, or
    /// never with `None`.
    struct FakeOps {
        git_dir: PathBuf,
        findings: Vec<&'static str>,
        fix_results: RefCell<Vec<bool>>,
        log: RefCell<Vec<String>>,
        prompts: RefCell<Vec<String>>,
        store_at_fix: RefCell<Vec<usize>>,
        fix_cap_reached: bool,
        /// Bot comments the first `import_bot_comments` call appends; later
        /// calls append nothing, as the dedup sidecar would.
        bot_comments: RefCell<Vec<NewComment>>,
        /// `log`'s length at each `import_bot_comments` call, so a test can
        /// place the import among the logged steps without changing them.
        import_at: RefCell<Vec<usize>>,
        signal_after: Option<u32>,
        /// Whether each signal file is first written half-way, as an agent
        /// that skipped the rename would leave it.
        half_written: bool,
        pending: RefCell<Vec<(u32, PathBuf, String)>>,
        elapsed: std::cell::Cell<i64>,
        sleeps: std::cell::Cell<u32>,
        states: RefCell<std::collections::HashMap<i64, StepState>>,
        signalled: RefCell<Vec<i64>>,
        completions: RefCell<Vec<String>>,
        /// Whether a fix pass writes its marker at all.
        fix_signals: bool,
        /// Whether `fix` reports an empty store (`None`).
        nothing_to_fix: bool,
        /// The `declined` array every fix pass writes into its marker.
        declined: &'static str,
    }

    impl FakeOps {
        fn new(git_dir: &Path, findings: Vec<&'static str>) -> Self {
            FakeOps {
                git_dir: git_dir.to_path_buf(),
                findings,
                fix_results: RefCell::new(Vec::new()),
                log: RefCell::new(Vec::new()),
                prompts: RefCell::new(Vec::new()),
                store_at_fix: RefCell::new(Vec::new()),
                fix_cap_reached: false,
                bot_comments: RefCell::new(Vec::new()),
                import_at: RefCell::new(Vec::new()),
                signal_after: Some(0),
                half_written: false,
                pending: RefCell::new(Vec::new()),
                elapsed: std::cell::Cell::new(0),
                sleeps: std::cell::Cell::new(0),
                states: RefCell::new(std::collections::HashMap::new()),
                signalled: RefCell::new(Vec::new()),
                completions: RefCell::new(Vec::new()),
                fix_signals: true,
                nothing_to_fix: false,
                declined: "[]",
            }
        }

        fn log(&self) -> Vec<String> {
            self.log.borrow().clone()
        }

        fn signal(&self, path: PathBuf, body: String) {
            if self.half_written {
                std::fs::write(&path, &body[..body.len() / 2]).unwrap();
            }
            match self.signal_after {
                Some(0) => std::fs::write(path, body).unwrap(),
                Some(sleeps) => self.pending.borrow_mut().push((sleeps, path, body)),
                None => {}
            }
        }
    }

    impl AutoReviewOps for FakeOps {
        fn review(&self, round: u32, prompt: String) -> Result<i64, AutoReviewError> {
            self.log.borrow_mut().push(format!("review {round}"));
            self.prompts.borrow_mut().push(prompt);
            let body = self.findings[(round - 1) as usize];
            self.signal(
                findings_path(&self.git_dir, LOOP_ID, round),
                body.to_string(),
            );
            Ok(100 + i64::from(round))
        }

        fn fix(&self, round: u32, completion: String) -> Result<Option<i64>, AutoReviewError> {
            self.log.borrow_mut().push("fix".to_string());
            self.completions.borrow_mut().push(completion);
            self.store_at_fix
                .borrow_mut()
                .push(comment_store::load(&self.git_dir).unwrap().len());
            let mut results = self.fix_results.borrow_mut();
            let succeeded = if results.is_empty() {
                true
            } else {
                results.remove(0)
            };
            if self.nothing_to_fix {
                return Ok(None);
            }
            if self.fix_signals {
                self.signal(
                    fix_marker_path(&self.git_dir, LOOP_ID, round),
                    format!(
                        r#"{{"succeeded": {succeeded}, "declined": {}}}"#,
                        self.declined
                    ),
                );
            }
            Ok(Some(200 + i64::from(round)))
        }

        fn step_state(&self, run_id: i64) -> Result<StepState, AutoReviewError> {
            Ok(self
                .states
                .borrow()
                .get(&run_id)
                .copied()
                .unwrap_or(StepState::Running))
        }

        fn mark_signalled(&self, run_id: i64, _step: &'static str) -> Result<(), AutoReviewError> {
            self.signalled.borrow_mut().push(run_id);
            Ok(())
        }

        fn sleep(&self, secs: u64) {
            self.elapsed.set(self.elapsed.get() + secs as i64);
            self.sleeps.set(self.sleeps.get() + 1);
            let mut pending = self.pending.borrow_mut();
            for (left, path, body) in pending.iter_mut() {
                *left -= 1;
                if *left == 0 {
                    std::fs::write(path, &*body).unwrap();
                }
            }
            pending.retain(|(left, _, _)| *left > 0);
        }

        fn publish(&self) -> Result<(), AutoReviewError> {
            self.log.borrow_mut().push("publish".to_string());
            Ok(())
        }

        fn record(&self, run_id: i64, kind: &str, detail: &str) -> Result<(), AutoReviewError> {
            self.log
                .borrow_mut()
                .push(format!("record {run_id} {kind} {detail}"));
            Ok(())
        }

        fn flag(&self, run_id: i64) -> Result<(), AutoReviewError> {
            self.log.borrow_mut().push(format!("flag {run_id}"));
            Ok(())
        }

        fn fix_cap_reached(&self) -> Result<bool, AutoReviewError> {
            Ok(self.fix_cap_reached)
        }

        fn import_bot_comments(&self) -> Result<usize, AutoReviewError> {
            self.import_at.borrow_mut().push(self.log.borrow().len());
            let comments = std::mem::take(&mut *self.bot_comments.borrow_mut());
            if !comments.is_empty() {
                comment_store::append(&self.git_dir, &comments, "2026-10-20T00:00:00Z")?;
            }
            Ok(comments.len())
        }

        fn now_unix_secs(&self) -> i64 {
            1_787_054_400 + self.elapsed.get()
        }
    }

    const LOOP_ID: &str = "42-1787054400";

    const MEDIUM: &str = r#"[{"path": "src/a.rs", "start_line": 4, "end_line": 6,
        "severity": "MEDIUM", "specialist": "state-flow", "body": "race"}]"#;
    const LOW: &str = r#"[{"path": "src/a.rs", "start_line": 9,
        "severity": "low", "specialist": "pedantic", "body": "typo"}]"#;

    fn cfg() -> AutoReviewConfig {
        AutoReviewConfig::default()
    }

    #[test]
    fn finding_to_comment_prefixes_provenance_and_severity() {
        let finding = Finding {
            path: "src/a.rs".to_string(),
            start_line: 4,
            end_line: None,
            severity: "high".to_string(),
            specialist: "economy".to_string(),
            body: "simplify".to_string(),
        };

        let comment = finding.to_comment();

        assert_eq!(comment.text, "[auto-review · economy · HIGH] simplify");
        assert_eq!((comment.start_line, comment.end_line), (4, 4));
    }

    #[test]
    fn an_unknown_severity_counts_as_low() {
        let finding: Vec<Finding> =
            serde_json::from_str(r#"[{"path":"a","start_line":1,"severity":"??","body":"b"}]"#)
                .unwrap();
        assert_eq!(finding[0].severity(), Severity::Low);
        assert_eq!(finding[0].specialist, "review");
    }

    #[test]
    fn try_read_findings_rejects_complete_json_of_the_wrong_shape() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("findings-1.json");
        for invalid in [
            r#"[{"path": "src/a.rs", "start_line": 1, "severity": "LOW"}]"#,
            r#"[{"path": "a", "start_line": "1", "severity": "LOW", "body": "b"}]"#,
            r#"{"findings": []}"#,
        ] {
            std::fs::write(&path, invalid).unwrap();
            assert!(
                matches!(
                    try_read_findings(&path),
                    Err(AutoReviewError::Findings { .. })
                ),
                "{invalid}"
            );
        }
        std::fs::write(&path, r#"{"succeeded": "yes"}"#).unwrap();
        assert!(read_fix_marker(&path).is_err());
    }

    #[test]
    fn a_findings_file_that_can_never_parse_flags_the_round_and_fails() {
        let tmp = TempDir::new().unwrap();
        let ops = FakeOps::new(tmp.path(), vec![r#"[{"path": "a"}]"#]);

        let err = run_loop(&ops, &cfg(), DEFAULT_PROMPT, "GH-1", tmp.path(), LOOP_ID).unwrap_err();

        assert!(matches!(err, AutoReviewError::Findings { .. }), "{err}");
        assert!(err.to_string().contains("missing field"), "{err}");
        assert_eq!(ops.log(), vec!["review 1", "flag 101"]);
        assert_eq!(
            ops.sleeps.get(),
            0,
            "no waiting for a file that cannot parse"
        );
    }

    #[test]
    fn render_prompt_appends_the_completion_section_even_to_a_custom_template() {
        let findings = Path::new("/g/vdiff/auto-review/findings-1.json");

        let prompt = render_prompt("Review {key} your own way.", "GH-1", 1, findings, None, &[]);

        assert!(prompt.starts_with("Review GH-1 your own way."));
        assert!(prompt.ends_with(&review_completion_section(findings)));
        assert!(prompt.contains("/g/vdiff/auto-review/findings-1.json.tmp"));
    }

    #[test]
    fn the_fix_completion_section_names_the_marker_and_its_temporary_name() {
        let marker = fix_marker_path(Path::new("/g"), "7-9", 2);
        assert_eq!(marker, Path::new("/g/vdiff/auto-review/fix-7-9-2.done"));

        let section = fix_completion_section(&marker);

        assert!(section.contains("`/g/vdiff/auto-review/fix-7-9-2.done.tmp`"));
        assert!(section.contains("`/g/vdiff/auto-review/fix-7-9-2.done`"));
        assert!(section.contains(r#"{"succeeded": true}"#));
        assert!(section.contains(r#"{"succeeded": false}"#));
        assert!(section.contains(r#""declined""#), "{section}");
        assert!(section.contains(r#""reason""#), "{section}");
    }

    #[test]
    fn the_fix_completion_section_asks_for_merged_comments_as_declines() {
        let section = fix_completion_section(Path::new("/g/fix.done"));

        assert!(section.contains("merged"), "{section}");
        assert!(section.contains("same root cause as"), "{section}");
    }

    #[test]
    fn try_read_findings_is_none_until_the_file_parses() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("findings-1.json");
        assert_eq!(try_read_findings(&path).unwrap(), None);

        std::fs::write(&path, "[{\"path\": \"src/a.rs\"").unwrap();
        assert_eq!(try_read_findings(&path).unwrap(), None, "half-written");

        std::fs::write(&path, "[]").unwrap();
        assert_eq!(try_read_findings(&path).unwrap(), Some(Vec::new()));
    }

    #[test]
    fn read_fix_marker_reads_succeeded_and_is_none_until_it_parses() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("fix-1.done");
        assert_eq!(read_fix_marker(&path).unwrap(), None);

        std::fs::write(&path, "{\"succ").unwrap();
        assert_eq!(read_fix_marker(&path).unwrap(), None);

        std::fs::write(&path, r#"{"succeeded": false}"#).unwrap();
        assert_eq!(
            read_fix_marker(&path).unwrap(),
            Some(FixMarker {
                succeeded: false,
                declined: Vec::new()
            })
        );

        std::fs::write(
            &path,
            r#"{"succeeded": true, "declined": [{"comment": "src/a.rs:4-6", "reason": "intended"}]}"#,
        )
        .unwrap();
        assert_eq!(
            read_fix_marker(&path).unwrap(),
            Some(FixMarker {
                succeeded: true,
                declined: vec![Decline {
                    comment: "src/a.rs:4-6".to_string(),
                    reason: "intended".to_string(),
                }]
            })
        );
    }

    #[test]
    fn render_prompt_uses_code_review_first_and_followup_with_the_archive_after() {
        let first = render_prompt(
            DEFAULT_PROMPT,
            "GH-1",
            1,
            Path::new("/g/f-1.json"),
            None,
            &[],
        );
        assert!(first.contains("`thatch-code-review`"));
        assert!(first.contains("/g/f-1.json"));
        assert!(first.contains("GH-1"));
        assert!(!first.contains("unattended"));
        assert_eq!(
            first.matches("/g/f-1.json`").count(),
            1,
            "only the completion section names the file: {first}"
        );
        assert!(!first.contains("prior round"));
        for placeholder in [
            "{{",
            "{key}",
            "{round}",
            "{skill}",
            "{findings_file}",
            "{prior}",
        ] {
            assert!(
                !first.contains(placeholder),
                "{placeholder} left in: {first}"
            );
        }

        let second = render_prompt(
            DEFAULT_PROMPT,
            "GH-1",
            2,
            Path::new("/g/f-2.json"),
            Some(Path::new("/g/archive/1.json")),
            &[],
        );
        assert!(second.contains("`thatch-review-followup`"));
        assert!(second.contains("/g/archive/1.json"));
        assert!(!second.contains("declined"), "{second}");
    }

    #[test]
    fn a_followup_prompt_lists_the_fix_pass_declines_with_their_reasons() {
        let declined = [Decline {
            comment: "src/a.rs:4-6".to_string(),
            reason: "the lock is held by the caller".to_string(),
        }];

        let second = render_prompt(
            DEFAULT_PROMPT,
            "GH-1",
            2,
            Path::new("/g/f-2.json"),
            Some(Path::new("/g/archive/1.json")),
            &declined,
        );

        assert!(second.contains("declined"), "{second}");
        assert!(
            second.contains("`src/a.rs:4-6`: the lock is held by the caller"),
            "{second}"
        );
        assert!(second.contains("re-report"), "{second}");
    }

    #[test]
    fn a_clean_first_round_writes_low_findings_and_stops_without_a_fix() {
        let tmp = TempDir::new().unwrap();
        let ops = FakeOps::new(tmp.path(), vec![LOW]);

        let outcome = run_loop(&ops, &cfg(), DEFAULT_PROMPT, "GH-1", tmp.path(), LOOP_ID).unwrap();

        assert_eq!(
            outcome,
            LoopOutcome {
                rounds: 1,
                verdict: RoundVerdict::Clean
            }
        );
        assert_eq!(
            ops.log(),
            vec!["review 1", "record 101 auto_review_round 1/2 clean"]
        );
        let store = comment_store::load(tmp.path()).unwrap();
        assert_eq!(store.len(), 1);
        assert_eq!(store[0]["text"], "[auto-review · pedantic · LOW] typo");
        assert_eq!(store[0]["created_at"], "2026-08-18T12:00:00Z");
    }

    #[test]
    fn a_finding_at_the_threshold_fixes_archives_and_runs_a_followup_round() {
        let tmp = TempDir::new().unwrap();
        let ops = FakeOps::new(tmp.path(), vec![MEDIUM, "[]"]);

        let outcome = run_loop(&ops, &cfg(), DEFAULT_PROMPT, "GH-1", tmp.path(), LOOP_ID).unwrap();

        assert_eq!(
            outcome,
            LoopOutcome {
                rounds: 2,
                verdict: RoundVerdict::Clean
            }
        );
        assert_eq!(
            ops.log(),
            vec![
                "review 1",
                "fix",
                "record 101 auto_review_round 1/2 fix",
                "review 2",
                "record 102 auto_review_round 2/2 clean",
            ]
        );
        assert_eq!(
            *ops.store_at_fix.borrow(),
            vec![1],
            "fix sees round 1's comment"
        );
        assert!(comment_store::load(tmp.path()).unwrap().is_empty());
        assert!(comment_store::archive_path(tmp.path(), 1).exists());
        let prompts = ops.prompts.borrow();
        assert!(prompts[1].contains(FOLLOWUP_SKILL));
        assert!(
            prompts[1].contains(&*comment_store::archive_path(tmp.path(), 1).to_string_lossy())
        );
    }

    #[test]
    fn the_next_round_is_told_what_the_fix_pass_declined() {
        let tmp = TempDir::new().unwrap();
        let mut ops = FakeOps::new(tmp.path(), vec![MEDIUM, "[]"]);
        ops.declined = r#"[{"comment": "src/a.rs:4-6", "reason": "no race: single-threaded"}]"#;

        run_loop(&ops, &cfg(), DEFAULT_PROMPT, "GH-1", tmp.path(), LOOP_ID).unwrap();

        let prompts = ops.prompts.borrow();
        assert!(!prompts[0].contains("single-threaded"));
        assert!(
            prompts[1].contains("`src/a.rs:4-6`: no race: single-threaded"),
            "{}",
            prompts[1]
        );
    }

    fn bot_comment(text: &str) -> NewComment {
        NewComment {
            path: "src/b.rs".to_string(),
            start_line: 2,
            end_line: 2,
            text: format!("[github · @cursor[bot]] {text}"),
        }
    }

    #[test]
    fn bot_comments_are_imported_right_before_each_fix_and_ride_along() {
        let tmp = TempDir::new().unwrap();
        let ops = FakeOps::new(tmp.path(), vec![MEDIUM, MEDIUM, "[]"]);
        *ops.bot_comments.borrow_mut() = vec![bot_comment("off by one")];
        let cfg = AutoReviewConfig {
            max_review_rounds: 3,
            ..cfg()
        };

        run_loop(&ops, &cfg, DEFAULT_PROMPT, "GH-1", tmp.path(), LOOP_ID).unwrap();

        assert_eq!(
            *ops.import_at.borrow(),
            vec![1, 4],
            "once after each fixing round's review, before its fix"
        );
        assert_eq!(
            *ops.store_at_fix.borrow(),
            vec![2, 1],
            "round 1's fix sees its finding plus the bot comment; round 2's \
             sees only its own, the bot thread having been imported once"
        );
    }

    #[test]
    fn bot_comments_never_trigger_a_fix() {
        let tmp = TempDir::new().unwrap();
        let ops = FakeOps::new(tmp.path(), vec!["[]"]);
        *ops.bot_comments.borrow_mut() = vec![bot_comment("off by one")];

        let outcome = run_loop(&ops, &cfg(), DEFAULT_PROMPT, "GH-1", tmp.path(), LOOP_ID).unwrap();

        assert_eq!(outcome.verdict, RoundVerdict::Clean);
        assert!(ops.import_at.borrow().is_empty());
        assert!(!ops.log().contains(&"fix".to_string()));
    }

    #[test]
    fn hitting_the_cap_flags_the_round_and_leaves_the_store() {
        let tmp = TempDir::new().unwrap();
        let ops = FakeOps::new(tmp.path(), vec![MEDIUM, MEDIUM]);

        let outcome = run_loop(&ops, &cfg(), DEFAULT_PROMPT, "GH-1", tmp.path(), LOOP_ID).unwrap();

        assert_eq!(
            outcome,
            LoopOutcome {
                rounds: 2,
                verdict: RoundVerdict::Capped
            }
        );
        assert_eq!(
            ops.log()[3..],
            [
                "review 2",
                "flag 102",
                "record 102 auto_review_round 2/2 capped"
            ]
        );
        assert_eq!(comment_store::load(tmp.path()).unwrap().len(), 1);
    }

    #[test]
    fn the_shared_fix_round_cap_stops_the_loop_before_a_fix() {
        let tmp = TempDir::new().unwrap();
        let mut ops = FakeOps::new(tmp.path(), vec![MEDIUM, MEDIUM]);
        ops.fix_cap_reached = true;

        let outcome = run_loop(&ops, &cfg(), DEFAULT_PROMPT, "GH-1", tmp.path(), LOOP_ID).unwrap();

        assert_eq!(
            outcome,
            LoopOutcome {
                rounds: 1,
                verdict: RoundVerdict::Capped
            }
        );
        assert_eq!(
            ops.log(),
            [
                "review 1",
                "flag 101",
                "record 101 auto_review_round 1/2 capped"
            ]
        );
        assert_eq!(comment_store::load(tmp.path()).unwrap().len(), 1);
    }

    #[test]
    fn existing_human_comments_are_kept_and_fed_to_the_fix() {
        let tmp = TempDir::new().unwrap();
        comment_store::append(
            tmp.path(),
            &[NewComment {
                path: "src/z.rs".to_string(),
                start_line: 1,
                end_line: 1,
                text: "human".to_string(),
            }],
            "2026-01-01T00:00:00Z",
        )
        .unwrap();
        let ops = FakeOps::new(tmp.path(), vec![MEDIUM, "[]"]);

        run_loop(&ops, &cfg(), DEFAULT_PROMPT, "GH-1", tmp.path(), LOOP_ID).unwrap();

        assert_eq!(*ops.store_at_fix.borrow(), vec![2]);
    }

    #[test]
    fn a_failed_fix_stops_the_loop() {
        let tmp = TempDir::new().unwrap();
        let ops = FakeOps::new(tmp.path(), vec![MEDIUM, "[]"]);
        ops.fix_results.borrow_mut().push(false);

        let outcome = run_loop(&ops, &cfg(), DEFAULT_PROMPT, "GH-1", tmp.path(), LOOP_ID).unwrap();

        assert_eq!(outcome.verdict, RoundVerdict::FixFailed);
        assert_eq!(
            ops.log(),
            vec![
                "review 1",
                "fix",
                "flag 201",
                "record 101 auto_review_round 1/2 fix-failed"
            ]
        );
        assert_eq!(
            comment_store::load(tmp.path()).unwrap().len(),
            1,
            "an unconsumed store is not archived"
        );
    }

    #[test]
    fn publish_runs_after_each_round_with_findings_when_enabled() {
        let tmp = TempDir::new().unwrap();
        let ops = FakeOps::new(tmp.path(), vec![MEDIUM, "[]"]);
        let cfg = AutoReviewConfig {
            publish: true,
            ..cfg()
        };

        run_loop(&ops, &cfg, DEFAULT_PROMPT, "GH-1", tmp.path(), LOOP_ID).unwrap();

        assert_eq!(ops.log()[..2], ["review 1", "publish"]);
        assert_eq!(
            ops.log().iter().filter(|l| *l == "publish").count(),
            1,
            "an empty round has nothing to publish"
        );
    }

    #[test]
    fn a_higher_threshold_lets_medium_findings_through_clean() {
        let tmp = TempDir::new().unwrap();
        let ops = FakeOps::new(tmp.path(), vec![MEDIUM]);
        let cfg = AutoReviewConfig {
            fix_threshold: Severity::High,
            ..cfg()
        };

        let outcome = run_loop(&ops, &cfg, DEFAULT_PROMPT, "GH-1", tmp.path(), LOOP_ID).unwrap();

        assert_eq!(outcome.verdict, RoundVerdict::Clean);
        assert!(!ops.log().contains(&"fix".to_string()));
    }

    #[test]
    fn steps_that_signal_after_a_delay_let_the_loop_advance_and_mark_their_rows() {
        let tmp = TempDir::new().unwrap();
        let mut ops = FakeOps::new(tmp.path(), vec![MEDIUM, "[]"]);
        ops.signal_after = Some(3);

        let outcome = run_loop(&ops, &cfg(), DEFAULT_PROMPT, "GH-1", tmp.path(), LOOP_ID).unwrap();

        assert_eq!(
            outcome,
            LoopOutcome {
                rounds: 2,
                verdict: RoundVerdict::Clean
            }
        );
        assert_eq!(*ops.signalled.borrow(), vec![101, 201, 102]);
        assert_eq!(ops.sleeps.get(), 9, "three polls' sleeps per step");
        assert!(
            ops.completions.borrow()[0]
                .contains(&*fix_marker_path(tmp.path(), LOOP_ID, 1).to_string_lossy())
        );
    }

    #[test]
    fn a_half_written_findings_file_is_retried_until_it_parses() {
        let tmp = TempDir::new().unwrap();
        let mut ops = FakeOps::new(tmp.path(), vec![LOW]);
        ops.signal_after = Some(2);
        ops.half_written = true;

        let outcome = run_loop(&ops, &cfg(), DEFAULT_PROMPT, "GH-1", tmp.path(), LOOP_ID).unwrap();

        assert_eq!(outcome.verdict, RoundVerdict::Clean);
        assert_eq!(comment_store::load(tmp.path()).unwrap().len(), 1);
    }

    #[test]
    fn a_round_that_never_signals_is_flagged_stuck_at_the_timeout() {
        let tmp = TempDir::new().unwrap();
        let mut ops = FakeOps::new(tmp.path(), vec![MEDIUM]);
        ops.signal_after = None;
        ops.half_written = true;
        let cfg = AutoReviewConfig {
            round_timeout_mins: 1,
            ..cfg()
        };

        let outcome = run_loop(&ops, &cfg, DEFAULT_PROMPT, "GH-1", tmp.path(), LOOP_ID).unwrap();

        assert_eq!(
            outcome,
            LoopOutcome {
                rounds: 1,
                verdict: RoundVerdict::Stuck
            }
        );
        assert_eq!(
            ops.log(),
            vec![
                "review 1",
                "flag 101",
                "record 101 auto_review_round 1/2 stuck"
            ]
        );
        assert_eq!(ops.elapsed.get(), 60);
        assert!(ops.signalled.borrow().is_empty());
    }

    #[test]
    fn a_zero_round_timeout_waits_without_a_limit() {
        let tmp = TempDir::new().unwrap();
        let mut ops = FakeOps::new(tmp.path(), vec![LOW]);
        // Well past what a one-minute timeout would allow.
        ops.signal_after = Some(500);
        let cfg = AutoReviewConfig {
            round_timeout_mins: 0,
            ..cfg()
        };

        let outcome = run_loop(&ops, &cfg, DEFAULT_PROMPT, "GH-1", tmp.path(), LOOP_ID).unwrap();

        assert_eq!(outcome.verdict, RoundVerdict::Clean);
        assert_eq!(ops.sleeps.get(), 500);
    }

    #[test]
    fn a_round_hibernated_before_its_signal_is_flagged_stuck() {
        let tmp = TempDir::new().unwrap();
        let mut ops = FakeOps::new(tmp.path(), vec![MEDIUM]);
        ops.signal_after = Some(5);
        ops.states.borrow_mut().insert(101, StepState::Hibernated);

        let outcome = run_loop(&ops, &cfg(), DEFAULT_PROMPT, "GH-1", tmp.path(), LOOP_ID).unwrap();

        assert_eq!(outcome.verdict, RoundVerdict::Stuck);
        assert_eq!(ops.log()[1], "flag 101");
        assert_eq!(ops.sleeps.get(), 0, "no waiting once hibernated");
    }

    #[test]
    fn a_round_hibernated_after_its_signal_counts_as_done() {
        let tmp = TempDir::new().unwrap();
        let ops = FakeOps::new(tmp.path(), vec![LOW]);
        ops.states.borrow_mut().insert(101, StepState::Hibernated);

        let outcome = run_loop(&ops, &cfg(), DEFAULT_PROMPT, "GH-1", tmp.path(), LOOP_ID).unwrap();

        assert_eq!(outcome.verdict, RoundVerdict::Clean);
        assert_eq!(*ops.signalled.borrow(), vec![101]);
    }

    #[test]
    fn a_round_ended_by_something_else_stops_the_loop_without_flagging() {
        let tmp = TempDir::new().unwrap();
        let mut ops = FakeOps::new(tmp.path(), vec![MEDIUM]);
        ops.signal_after = None;
        ops.states.borrow_mut().insert(101, StepState::Gone);

        let outcome = run_loop(&ops, &cfg(), DEFAULT_PROMPT, "GH-1", tmp.path(), LOOP_ID).unwrap();

        assert_eq!(outcome.verdict, RoundVerdict::Stopped);
        assert_eq!(
            ops.log(),
            vec!["review 1", "record 101 auto_review_round 1/2 stopped"]
        );
    }

    #[test]
    fn a_fix_pass_that_never_signals_is_flagged_and_stops_the_round_stuck() {
        let tmp = TempDir::new().unwrap();
        let mut ops = FakeOps::new(tmp.path(), vec![MEDIUM, "[]"]);
        ops.fix_signals = false;
        // A marker an earlier loop's agent writes must not count as this
        // pass's.
        let earlier = fix_marker_path(tmp.path(), "1-1", 1);
        std::fs::create_dir_all(earlier.parent().unwrap()).unwrap();
        std::fs::write(earlier, r#"{"succeeded": true}"#).unwrap();
        let cfg = AutoReviewConfig {
            round_timeout_mins: 1,
            ..cfg()
        };

        let outcome = run_loop(&ops, &cfg, DEFAULT_PROMPT, "GH-1", tmp.path(), LOOP_ID).unwrap();

        assert_eq!(outcome.verdict, RoundVerdict::Stuck);
        assert_eq!(
            ops.log(),
            vec![
                "review 1",
                "fix",
                "flag 201",
                "record 101 auto_review_round 1/2 stuck"
            ]
        );
        assert_eq!(*ops.signalled.borrow(), vec![101]);
    }

    #[test]
    fn a_fix_pass_ended_by_something_else_stops_the_loop_without_flagging() {
        let tmp = TempDir::new().unwrap();
        let mut ops = FakeOps::new(tmp.path(), vec![MEDIUM, "[]"]);
        ops.fix_signals = false;
        ops.states.borrow_mut().insert(201, StepState::Gone);

        let outcome = run_loop(&ops, &cfg(), DEFAULT_PROMPT, "GH-1", tmp.path(), LOOP_ID).unwrap();

        assert_eq!(outcome.verdict, RoundVerdict::Stopped);
        assert!(!ops.log().iter().any(|line| line.starts_with("flag")));
    }

    #[test]
    fn a_fix_with_nothing_to_fix_counts_as_fixed_without_waiting() {
        let tmp = TempDir::new().unwrap();
        let mut ops = FakeOps::new(tmp.path(), vec![MEDIUM, "[]"]);
        ops.nothing_to_fix = true;

        let outcome = run_loop(&ops, &cfg(), DEFAULT_PROMPT, "GH-1", tmp.path(), LOOP_ID).unwrap();

        assert_eq!(outcome.verdict, RoundVerdict::Clean);
        assert_eq!(*ops.signalled.borrow(), vec![101, 102]);
    }
}
