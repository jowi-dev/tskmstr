//! `tm pr watch`'s poll-loop core: `docs/plans/bugbot-watch.md`'s "Poll loop
//! mechanics" and "Findings-to-prompt plumbing" sections.
//!
//! [`poll_once`] is one tick's worth of pure-ish decision logic (given the
//! injected [`GhCli`]/[`RunStore`], no real sleeping): check the PR's
//! lifecycle, then whether every configured bot has reviewed, and finish the
//! tracked run the moment either check resolves to a terminal state.
//! [`run_poll_loop`] drives [`poll_once`] in a real loop, sleeping via the
//! injected [`Sleeper`] between non-terminal ticks, applying the
//! consecutive-`gh`-failure backoff and the wall-clock give-up timeout.
//!
//! # What "bots done" resolves to, in one tick
//!
//! Per the plan, "bots done" is itself terminal — the moment
//! [`crate::github::bot_findings::bots_have_reviewed`] is true, this tick
//! tallies findings and finishes the run (`Done` for zero findings, `Review`
//! for unresolved ones), so there is no separate "first time seen" state to
//! track across ticks: every tick either isn't done yet (`Continue`) or is
//! done and immediately terminal (`Finished`).
//!
//! # Seams
//!
//! - [`Clock`]/[`Sleeper`] stand in for wall-clock time and real sleeping, so
//!   [`run_poll_loop`]'s give-up-timeout and backoff paths are exercised in
//!   tests without a real 24-hour wait.
//! - [`CleanupLauncher`] stands in for launching the bugbot-cleanup session
//!   when `on_bots_done == Launch`. `src/work/bugbot.rs::launch_cleanup` is
//!   the real implementation; `tm pr watch`'s CLI wiring (`src/main.rs`)
//!   passes `bugbot::RealCleanupLauncher`, an adapter over it.

use std::io::Write;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::config::{OnBotsDone, ReviewWatchConfig};
use crate::github::bot_findings::{
    FindingDetail, bot_finding_details, bots_have_reviewed, count_bot_findings,
};
use crate::github::gh_cli::{GhCli, GhError, PrLifecycle};
use crate::runs::{FinishRun, RunStatus, RunStore, RunStoreError};

/// Errors from one [`poll_once`] tick: either a `gh` shell-out failed, a
/// [`RunStore`] write failed, or writing the findings file failed. Every
/// variant counts toward [`run_poll_loop`]'s consecutive-failure backoff —
/// the plan's "gh failures" language covers the shell-out specifically, but
/// a local sqlite hiccup deserves exactly the same tolerant retry, not a
/// silently swallowed `let _ =` that would mask a real bug.
#[derive(Debug, Error)]
pub enum PollError {
    /// A `gh` shell-out failed.
    #[error(transparent)]
    Gh(#[from] GhError),

    /// A [`RunStore`] operation failed.
    #[error(transparent)]
    Store(#[from] RunStoreError),

    /// The findings file could not be written.
    #[error("failed to write findings file {path}: {source}")]
    FindingsFile {
        /// Path that could not be written.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

/// A wall-clock time source, abstracted so [`run_poll_loop`]'s give-up
/// timeout is testable without a real 24-hour wait. Deliberately a different
/// shape from [`crate::work::run::Clock`] (broken-down local time for
/// filename timestamps) — this only ever needs a monotonically-comparable
/// "now", so plain Unix seconds is simpler for elapsed-time arithmetic.
pub trait Clock {
    /// Seconds since the Unix epoch, UTC.
    fn now_unix_secs(&self) -> i64;
}

/// Production [`Clock`], backed by [`std::time::SystemTime`].
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix_secs(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }
}

/// A sleep abstraction, so [`run_poll_loop`]'s tests never actually sleep.
pub trait Sleeper {
    /// Sleep for `secs` seconds.
    fn sleep(&self, secs: u64);
}

/// Production [`Sleeper`], backed by [`std::thread::sleep`].
#[derive(Debug, Default, Clone, Copy)]
pub struct RealSleeper;

impl Sleeper for RealSleeper {
    fn sleep(&self, secs: u64) {
        std::thread::sleep(std::time::Duration::from_secs(secs));
    }
}

/// Seam for launching (or attaching to) the bugbot-cleanup session once a
/// tick finds unresolved bot findings and `on_bots_done == Launch`. The real
/// implementation is `src/work/bugbot.rs::launch_cleanup`
/// (`docs/plans/bugbot-watch.md` step 10, not yet implemented).
///
/// Failures are the implementation's own problem to report: [`poll_once`]
/// has already committed the run to [`RunStatus::Review`] by the time this
/// is called, so a launch failure here must not un-finish the run or count
/// toward [`run_poll_loop`]'s failure backoff.
pub trait CleanupLauncher {
    /// Launch (or attach to) the cleanup session for ticket `key`.
    fn launch_cleanup(&self, key: &str);
}

/// Dependencies [`poll_once`]/[`run_poll_loop`] need, gathered so callers
/// don't have to thread five separate parameters through every call, mirroring
/// [`crate::work::run::RunLaneDeps`]'s shape.
pub struct PollDeps<'a> {
    /// `gh` CLI operations (real or fake).
    pub gh: &'a dyn GhCli,
    /// The run-state store `add_event`/`finish_run` are called against.
    pub store: &'a RunStore,
    /// "Now" source for the give-up timeout.
    pub clock: &'a dyn Clock,
    /// Sleep between ticks (real or fake).
    pub sleeper: &'a dyn Sleeper,
    /// Cleanup-session launch seam, invoked when `on_bots_done == Launch`.
    pub cleanup_launcher: &'a dyn CleanupLauncher,
}

/// Everything one tick (or the driving loop) needs to know about *this*
/// watch, gathered the same way [`crate::work::run::RunLanePaths`] gathers
/// per-run filesystem inputs separately from behavioral seams.
pub struct PollRequest<'a> {
    /// The already-created `review-watch` run row's id (created by the CLI
    /// layer's `start_run` before the loop starts — see
    /// `docs/plans/bugbot-watch.md`'s "CLI surface").
    pub run_id: i64,
    /// The ticket key this watch is for, e.g. `PROJ-372`.
    pub ticket: &'a str,
    /// The pull request number being watched.
    pub pr_number: u64,
    /// The repository root every `gh` call in this loop is shelled out
    /// against (`.current_dir(repo_root)`) — see
    /// [`crate::cli::pr::resolve_watch_repo_root`]'s doc comment for how
    /// this is resolved. Necessary because this loop runs detached, with no
    /// guarantee its own ambient cwd is the ticket's repo.
    pub repo_root: &'a Path,
    /// Configured bot logins (`[review_bots]`), reused as-is for both the
    /// "bots have reviewed" predicate and the findings tally.
    pub bot_logins: &'a [String],
    /// Validated `[work.review_watch]` config (poll cadence, give-up
    /// timeout, `on_bots_done`).
    pub config: &'a ReviewWatchConfig,
    /// Unix-seconds timestamp this watch started at, for the give-up
    /// timeout. The CLI layer takes this from [`Clock::now_unix_secs`] at
    /// `start_run` time, not from the run row's own `started_at` (which is a
    /// SQL-formatted string, not epoch seconds) — negligible drift, since
    /// the loop starts moments after the row is created in the same
    /// foreground process.
    pub started_at_unix: i64,
    /// The invoking user's home directory, for the findings-file path
    /// fallback (see [`findings_file_path`]).
    pub home: &'a Path,
    /// `$XDG_DATA_HOME`, if set, for the findings-file path.
    pub xdg_data_home: Option<&'a Path>,
}

/// What one [`poll_once`] tick decided.
#[derive(Debug)]
pub enum TickOutcome {
    /// Nothing terminal happened this tick (the PR is still open and bots
    /// haven't all reviewed yet); the caller should sleep and try again.
    Continue,
    /// The watch reached a terminal state this tick; the run has already
    /// been finished in the store.
    Finished(PollOutcome),
}

/// The final outcome of a `tm pr watch --foreground` invocation, mapped by
/// the CLI layer to exit codes `0`/`1`/`2` respectively.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollOutcome {
    /// The PR closed/merged, or bots finished (with or without findings) —
    /// every "the watcher did its job" case.
    Handled,
    /// Gave up after [`run_poll_loop`]'s consecutive-`gh`-failure limit.
    Failed,
    /// Gave up after `config.max_wait_mins` elapsed with the PR still open
    /// and bots not done.
    GaveUp,
}

/// One tick's worth of poll-loop logic:
///
/// 1. [`GhCli::pr_state`]: `Merged`/`Closed` emits `pr_closed` (detail
///    `{"reason": "merged"|"closed"}`), finishes the run `Done`, and returns
///    [`TickOutcome::Finished`].
/// 2. Otherwise, [`bots_have_reviewed`] over [`GhCli::pr_reviews`]. Not yet →
///    emits a heartbeat-only `bot_poll` event and returns
///    [`TickOutcome::Continue`].
/// 3. Bots done → [`GhCli::pr_review_threads`] + [`count_bot_findings`] for
///    the tally, emitting `bots_done` (detail `{"total": N, "unresolved":
///    N}`).
///    - Zero unresolved → finishes the run `Done`.
///    - Otherwise → [`GhCli::pr_bot_finding_details`] + [`bot_finding_details`]
///      to filter to unresolved bot findings, writes the findings file (see
///      [`findings_file_path`]), finishes the run `Review`, and — when
///      `config.on_bots_done == Launch` — invokes
///      [`PollDeps::cleanup_launcher`].
///
/// Any `gh`/store/filesystem failure along the way propagates as a
/// [`PollError`] rather than being swallowed, so [`run_poll_loop`] can log it
/// and count it toward the give-up backoff.
pub fn poll_once(
    deps: &PollDeps<'_>,
    req: &PollRequest<'_>,
    out: &mut dyn Write,
) -> Result<TickOutcome, PollError> {
    let now = deps.clock.now_unix_secs();
    let lifecycle = deps.gh.pr_state(req.repo_root, req.pr_number)?;
    if matches!(lifecycle, PrLifecycle::Merged | PrLifecycle::Closed) {
        let reason = if lifecycle == PrLifecycle::Merged {
            "merged"
        } else {
            "closed"
        };
        let detail = serde_json::json!({ "reason": reason }).to_string();
        deps.store
            .add_event(req.run_id, "pr_closed", Some(&detail))?;
        log_event(out, now, "pr_closed", Some(&detail));
        deps.store.finish_run(
            req.run_id,
            &FinishRun {
                status: RunStatus::Done,
                ..FinishRun::default()
            },
        )?;
        return Ok(TickOutcome::Finished(PollOutcome::Handled));
    }

    let reviews = deps.gh.pr_reviews(req.repo_root, req.pr_number)?;
    if !bots_have_reviewed(&reviews, req.bot_logins) {
        deps.store.add_event(req.run_id, "bot_poll", None)?;
        log_event(out, now, "bot_poll", None);
        return Ok(TickOutcome::Continue);
    }

    let threads = deps.gh.pr_review_threads(req.repo_root, req.pr_number)?;
    let counts = count_bot_findings(&threads, req.bot_logins);
    let bots_done_detail =
        serde_json::json!({ "total": counts.total, "unresolved": counts.unresolved }).to_string();
    deps.store
        .add_event(req.run_id, "bots_done", Some(&bots_done_detail))?;
    log_event(out, now, "bots_done", Some(&bots_done_detail));

    if counts.unresolved == 0 {
        deps.store.finish_run(
            req.run_id,
            &FinishRun {
                status: RunStatus::Done,
                ..FinishRun::default()
            },
        )?;
        return Ok(TickOutcome::Finished(PollOutcome::Handled));
    }

    let details = deps
        .gh
        .pr_bot_finding_details(req.repo_root, req.pr_number)?;
    let findings = bot_finding_details(&details, req.bot_logins);
    let path = findings_file_path(req.home, req.xdg_data_home, req.ticket);
    write_findings_file(&path, &findings)?;
    log_event(
        out,
        now,
        "findings_written",
        Some(&serde_json::json!({ "path": path.display().to_string() }).to_string()),
    );

    deps.store.finish_run(
        req.run_id,
        &FinishRun {
            status: RunStatus::Review,
            ..FinishRun::default()
        },
    )?;

    if req.config.on_bots_done == OnBotsDone::Launch {
        deps.cleanup_launcher.launch_cleanup(req.ticket);
    }

    Ok(TickOutcome::Finished(PollOutcome::Handled))
}

/// Drives [`poll_once`] until it reaches a terminal state, sleeping via
/// [`PollDeps::sleeper`] between non-terminal ticks. Two ways to give up
/// early, per `docs/plans/bugbot-watch.md`'s "Poll loop mechanics" 5-6:
///
/// - **Wall-clock timeout**: checked at the top of every iteration (cheap,
///   no `gh` call needed) — once `config.max_wait_mins` has elapsed since
///   `req.started_at_unix`, emits `give_up`, finishes the run `Failed`, and
///   returns [`PollOutcome::GaveUp`]. By construction this can only trigger
///   while the PR is still open and bots aren't done: either condition is
///   itself terminal (`Finished`) the same tick it's discovered, so a loop
///   still running has neither.
/// - **Consecutive `gh` failures**: each [`PollError`] from [`poll_once`]
///   emits a `poll_error` event and bumps a counter; after 10 in a row
///   (roughly 7-8 minutes of backoff at the default 45s cadence), finishes
///   the run `Failed` and returns [`PollOutcome::Failed`]. Any intervening
///   [`TickOutcome::Continue`] resets the counter — a single blip must not
///   accumulate toward an unrelated later blip.
pub fn run_poll_loop(
    deps: &PollDeps<'_>,
    req: &PollRequest<'_>,
    out: &mut dyn Write,
) -> PollOutcome {
    const MAX_CONSECUTIVE_FAILURES: u32 = 10;
    let mut consecutive_failures: u32 = 0;

    log_event(
        out,
        deps.clock.now_unix_secs(),
        "watch_started",
        Some(&serde_json::json!({ "ticket": req.ticket, "pr": req.pr_number }).to_string()),
    );

    loop {
        let now = deps.clock.now_unix_secs();
        let elapsed_secs = now - req.started_at_unix;
        if elapsed_secs > (req.config.max_wait_mins as i64) * 60 {
            let _ = deps.store.add_event(req.run_id, "give_up", None);
            log_event(out, now, "give_up", None);
            let _ = deps.store.finish_run(
                req.run_id,
                &FinishRun {
                    status: RunStatus::Failed,
                    ..FinishRun::default()
                },
            );
            log_event(out, now, "watch_finished", Some(r#"{"outcome":"gave_up"}"#));
            return PollOutcome::GaveUp;
        }

        match poll_once(deps, req, out) {
            Ok(TickOutcome::Continue) => {
                consecutive_failures = 0;
                deps.sleeper.sleep(req.config.poll_secs);
            }
            Ok(TickOutcome::Finished(outcome)) => {
                log_event(
                    out,
                    deps.clock.now_unix_secs(),
                    "watch_finished",
                    Some(&serde_json::json!({ "outcome": format!("{outcome:?}") }).to_string()),
                );
                return outcome;
            }
            Err(err) => {
                let detail = serde_json::json!({ "message": err.to_string() }).to_string();
                let _ = deps
                    .store
                    .add_event(req.run_id, "poll_error", Some(&detail));
                log_event(out, deps.clock.now_unix_secs(), "poll_error", Some(&detail));
                consecutive_failures += 1;
                if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                    let _ = deps.store.finish_run(
                        req.run_id,
                        &FinishRun {
                            status: RunStatus::Failed,
                            ..FinishRun::default()
                        },
                    );
                    log_event(
                        out,
                        deps.clock.now_unix_secs(),
                        "watch_finished",
                        Some(r#"{"outcome":"failed"}"#),
                    );
                    return PollOutcome::Failed;
                }
                deps.sleeper.sleep(req.config.poll_secs);
            }
        }
    }
}

/// Formats `unix_secs` as `YYYY-MM-DD HH:MM:SSZ` (UTC), for the
/// human-readable lines [`log_event`] writes to a detached watch's log file.
/// Deliberately UTC (`libc::gmtime`, not `localtime`) since a detached
/// watcher may run under a differently-configured `TZ` than whoever later
/// reads the log.
fn format_log_ts(unix_secs: i64) -> String {
    // SAFETY: `gmtime` takes a valid `time_t` pointer (a local, non-null
    // stack value) and returns a pointer into a `libc`-owned static `tm`
    // struct, which is copied out by value below before any other libc call
    // can overwrite it — no reference escapes this function.
    unsafe {
        let t: libc::time_t = unix_secs as libc::time_t;
        let tm_ptr = libc::gmtime(&t);
        if tm_ptr.is_null() {
            return format!("unix:{unix_secs}");
        }
        let tm = *tm_ptr;
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}Z",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}

/// Writes one timestamped, human-readable line to `out`, mirroring a
/// `run_events` row this call site also recorded via [`RunStore::add_event`]
/// (or is about to, for the terminal `finish_run` cases). Additive, never a
/// replacement: the SQLite event stays the durable record `tm runs show`
/// reads; this line exists so a detached watcher's log file (see
/// `crate::cli::pr::watch_log_dir`) shows *something* instead of sitting at
/// zero bytes, which is exactly the gap that left the owner unable to
/// diagnose a failed `tm pr watch` cron.
///
/// Best-effort: a write failure here (e.g. a full disk) must not fail the
/// tick or the loop — [`PollDeps::store`]'s SQLite row is what actually
/// matters, and `out` is only a convenience mirror of it — so failures are
/// silently swallowed, matching this module's existing `let _ =` stance on
/// [`RunStore::add_event`] calls in [`run_poll_loop`]'s give-up/backoff
/// paths.
fn log_event(out: &mut dyn Write, now_unix_secs: i64, kind: &str, detail: Option<&str>) {
    let ts = format_log_ts(now_unix_secs);
    match detail {
        Some(detail) => {
            let _ = writeln!(out, "[{ts}] {kind} {detail}");
        }
        None => {
            let _ = writeln!(out, "[{ts}] {kind}");
        }
    }
}

/// One findings-file entry: `{file, line, body, url}`, the shape
/// `/bugbot-triage`'s `{findings_file}` argument reads. A projection of
/// [`FindingDetail`] (drops `author_login`/`is_resolved`, already implied by
/// this file only ever containing unresolved bot findings).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct FindingFileEntry {
    file: Option<String>,
    line: Option<i64>,
    body: String,
    url: String,
}

impl From<&FindingDetail> for FindingFileEntry {
    fn from(detail: &FindingDetail) -> Self {
        FindingFileEntry {
            file: detail.path.clone(),
            line: detail.line,
            body: detail.body.clone(),
            url: detail.url.clone(),
        }
    }
}

/// The findings-file path for ticket `key`:
/// `${XDG_DATA_HOME:-~/.local/share}/tskmstr/findings/<lowercased key>.json`.
/// Mirrors [`crate::runs::default_db_path`]/[`crate::runs::session::sessions_dir`]'s
/// XDG resolution (same base directory, `findings` instead of
/// `runs.db`/`sessions`), for the same reason: a pure, directly testable
/// function so callers don't need a real `$XDG_DATA_HOME` to exercise the
/// path logic.
pub fn findings_file_path(home: &Path, xdg_data_home: Option<&Path>, key: &str) -> PathBuf {
    let base = match xdg_data_home {
        Some(xdg) => xdg.join("tskmstr"),
        None => home.join(".local").join("share").join("tskmstr"),
    };
    base.join("findings")
        .join(format!("{}.json", key.to_lowercase()))
}

/// Writes `findings` to `path` as a JSON array of `{file, line, body, url}`
/// objects, creating `path`'s parent directory if needed.
fn write_findings_file(path: &Path, findings: &[FindingDetail]) -> Result<(), PollError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| PollError::FindingsFile {
            path: path.to_path_buf(),
            source,
        })?;
    }

    let entries: Vec<FindingFileEntry> = findings.iter().map(FindingFileEntry::from).collect();
    let json = serde_json::to_string_pretty(&entries)
        .expect("Vec<FindingFileEntry> of plain owned strings/numbers always serializes");
    std::fs::write(path, json).map_err(|source| PollError::FindingsFile {
        path: path.to_path_buf(),
        source,
    })
}

/// A [`Clock`] test double returning a fixed, settable value.
///
/// A plain public struct (not `#[cfg(test)]`-gated), matching
/// [`crate::github::gh_cli::FakeGhCli`]'s convention, so other test code in
/// the crate (e.g. `cli::pr`'s `tm pr watch` tests) can depend on it
/// directly.
#[derive(Debug)]
pub struct FakeClock {
    current: std::cell::Cell<i64>,
    step: i64,
}

impl FakeClock {
    /// A clock that always reports `secs`.
    pub fn at(secs: i64) -> Self {
        FakeClock {
            current: std::cell::Cell::new(secs),
            step: 0,
        }
    }

    /// A clock that reports `start` on its first call, then advances by
    /// `step` seconds on every subsequent call. For tests where wall time
    /// needs to visibly pass between two calls (e.g. a caller capturing
    /// `started_at_unix` from one call, then [`run_poll_loop`] checking
    /// elapsed time against it on the next).
    pub fn advancing(start: i64, step: i64) -> Self {
        FakeClock {
            current: std::cell::Cell::new(start),
            step,
        }
    }
}

impl Clock for FakeClock {
    fn now_unix_secs(&self) -> i64 {
        let value = self.current.get();
        self.current.set(value + self.step);
        value
    }
}

/// A [`Sleeper`] test double that records calls instead of sleeping. Plain
/// public struct, same rationale as [`FakeClock`].
#[derive(Debug, Default)]
pub struct FakeSleeper {
    calls: std::cell::RefCell<Vec<u64>>,
}

impl FakeSleeper {
    /// The `secs` arguments passed to `sleep`, in call order.
    pub fn calls(&self) -> Vec<u64> {
        self.calls.borrow().clone()
    }
}

impl Sleeper for FakeSleeper {
    fn sleep(&self, secs: u64) {
        self.calls.borrow_mut().push(secs);
    }
}

/// A [`CleanupLauncher`] test double that records calls instead of launching
/// anything. Plain public struct, same rationale as [`FakeClock`].
#[derive(Debug, Default)]
pub struct FakeCleanupLauncher {
    calls: std::cell::RefCell<Vec<String>>,
}

impl FakeCleanupLauncher {
    /// The ticket keys passed to `launch_cleanup`, in call order.
    pub fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
}

impl CleanupLauncher for FakeCleanupLauncher {
    fn launch_cleanup(&self, key: &str) {
        self.calls.borrow_mut().push(key.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::bot_findings::PrReview;
    use crate::github::gh_cli::FakeGhCli;
    use crate::runs::{RunStore, StartRun};
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn open_store(dir: &Path) -> RunStore {
        RunStore::open(&dir.join("runs.db")).expect("open should succeed")
    }

    fn cursor_bot() -> Vec<String> {
        vec!["cursor[bot]".to_string()]
    }

    fn config() -> ReviewWatchConfig {
        ReviewWatchConfig::default()
    }

    fn start_watch_run(store: &RunStore, ticket: &str) -> i64 {
        store
            .start_run(&StartRun {
                ticket: ticket.to_string(),
                lane: "review-watch".to_string(),
                worktree: "/irrelevant".to_string(),
                branch: None,
                pid: Some(4242),
                kind: "review-watch".to_string(),
            })
            .unwrap()
    }

    struct Fixture {
        _db_dir: tempfile::TempDir,
        store: RunStore,
        gh: FakeGhCli,
        clock: FakeClock,
        sleeper: FakeSleeper,
        cleanup: FakeCleanupLauncher,
        home: PathBuf,
        run_id: i64,
        cfg: ReviewWatchConfig,
    }

    impl Fixture {
        fn new() -> Self {
            let db_dir = tempdir().unwrap();
            let store = open_store(db_dir.path());
            let run_id = start_watch_run(&store, "PROJ-1");
            Fixture {
                _db_dir: db_dir,
                store,
                gh: FakeGhCli::new(),
                clock: FakeClock::at(1_000),
                sleeper: FakeSleeper::default(),
                cleanup: FakeCleanupLauncher::default(),
                home: PathBuf::from("/Users/jowi"),
                run_id,
                cfg: config(),
            }
        }

        fn deps(&self) -> PollDeps<'_> {
            PollDeps {
                gh: &self.gh,
                store: &self.store,
                clock: &self.clock,
                sleeper: &self.sleeper,
                cleanup_launcher: &self.cleanup,
            }
        }

        fn req(&self) -> PollRequest<'_> {
            PollRequest {
                run_id: self.run_id,
                ticket: "PROJ-1",
                pr_number: 42,
                repo_root: &self.home,
                bot_logins: &[],
                config: &self.cfg,
                started_at_unix: 1_000,
                home: &self.home,
                xdg_data_home: None,
            }
        }
    }

    // --- poll_once: pr lifecycle ---

    #[test]
    fn poll_once_merged_pr_finishes_done_and_emits_pr_closed_merged() {
        let mut fx = Fixture::new();
        fx.gh = fx.gh.with_pr_state(42, Ok(PrLifecycle::Merged));

        let outcome = poll_once(&fx.deps(), &fx.req(), &mut Vec::new()).unwrap();

        assert!(matches!(
            outcome,
            TickOutcome::Finished(PollOutcome::Handled)
        ));
        let run = fx.store.run_by_id(fx.run_id).unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Done);

        let events = fx.store.events_for_run(fx.run_id).unwrap();
        let closed = events.iter().find(|e| e.kind == "pr_closed").unwrap();
        assert_eq!(closed.detail.as_deref(), Some(r#"{"reason":"merged"}"#));
    }

    // --- log content: the actual gap this module's log lines fix ---

    #[test]
    fn poll_once_merged_pr_writes_a_human_readable_pr_closed_line_to_out() {
        let mut fx = Fixture::new();
        fx.gh = fx.gh.with_pr_state(42, Ok(PrLifecycle::Merged));
        let mut out = Vec::new();

        poll_once(&fx.deps(), &fx.req(), &mut out).unwrap();

        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("pr_closed"),
            "expected a pr_closed line, got: {text:?}"
        );
        assert!(
            text.contains(r#"{"reason":"merged"}"#),
            "expected the same detail as the sqlite event, got: {text:?}"
        );
    }

    #[test]
    fn poll_once_not_done_writes_a_bot_poll_line_to_out() {
        let mut fx = Fixture::new();
        fx.gh = fx
            .gh
            .with_pr_state(42, Ok(PrLifecycle::Open))
            .with_pr_reviews(42, Ok(vec![]));
        let bots = cursor_bot();
        let mut req = fx.req();
        req.bot_logins = &bots;
        let mut out = Vec::new();

        poll_once(&fx.deps(), &req, &mut out).unwrap();

        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("bot_poll"), "got: {text:?}");
    }

    #[test]
    fn run_poll_loop_poll_error_writes_a_line_per_failure_to_out() {
        let mut fx = Fixture::new();
        fx.gh = fx.gh.with_pr_state(
            42,
            Err(GhError::Command {
                command: "gh pr view".to_string(),
                exit_code: Some(1),
                stderr: "boom".to_string(),
            }),
        );
        let mut out = Vec::new();

        let outcome = run_poll_loop(&fx.deps(), &fx.req(), &mut out);

        assert_eq!(outcome, PollOutcome::Failed);
        let text = String::from_utf8(out).unwrap();
        let error_lines = text.matches("poll_error").count();
        assert_eq!(
            error_lines, 10,
            "expected one log line per consecutive gh failure, got: {text:?}"
        );
        assert!(text.contains("boom"), "got: {text:?}");
        assert!(
            text.contains("watch_finished"),
            "expected a terminal line, got: {text:?}"
        );
    }

    #[test]
    fn run_poll_loop_wall_clock_timeout_writes_a_give_up_line_to_out() {
        let mut fx = Fixture::new();
        fx.cfg.max_wait_mins = 10;
        fx.clock = FakeClock::at(1_000 + 10 * 60 + 1);
        let mut out = Vec::new();

        let outcome = run_poll_loop(&fx.deps(), &fx.req(), &mut out);

        assert_eq!(outcome, PollOutcome::GaveUp);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("give_up"), "got: {text:?}");
    }

    #[test]
    fn format_log_ts_renders_utc_calendar_time() {
        // 2026-08-11T00:00:00Z, per any epoch converter.
        assert_eq!(format_log_ts(1_786_406_400), "2026-08-11 00:00:00Z");
    }

    #[test]
    fn poll_once_closed_unmerged_pr_finishes_done_and_emits_pr_closed_closed() {
        let mut fx = Fixture::new();
        fx.gh = fx.gh.with_pr_state(42, Ok(PrLifecycle::Closed));

        let outcome = poll_once(&fx.deps(), &fx.req(), &mut Vec::new()).unwrap();

        assert!(matches!(
            outcome,
            TickOutcome::Finished(PollOutcome::Handled)
        ));
        let events = fx.store.events_for_run(fx.run_id).unwrap();
        let closed = events.iter().find(|e| e.kind == "pr_closed").unwrap();
        assert_eq!(closed.detail.as_deref(), Some(r#"{"reason":"closed"}"#));
    }

    // --- poll_once: not done yet ---

    #[test]
    fn poll_once_not_done_emits_bot_poll_event_and_continues() {
        let mut fx = Fixture::new();
        fx.gh = fx
            .gh
            .with_pr_state(42, Ok(PrLifecycle::Open))
            .with_pr_reviews(42, Ok(vec![]));
        let bots = cursor_bot();
        let mut req = fx.req();
        req.bot_logins = &bots;

        let outcome = poll_once(&fx.deps(), &req, &mut Vec::new()).unwrap();

        assert!(matches!(outcome, TickOutcome::Continue));
        let run = fx.store.run_by_id(fx.run_id).unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Running, "must not finish the run");
        let events = fx.store.events_for_run(fx.run_id).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "bot_poll");
        assert_eq!(events[0].detail, None);
    }

    // --- poll_once: bots done ---

    #[test]
    fn poll_once_bots_done_zero_findings_finishes_done() {
        let bots = cursor_bot();
        let mut fx = Fixture::new();
        fx.gh = fx
            .gh
            .with_pr_state(42, Ok(PrLifecycle::Open))
            .with_pr_reviews(
                42,
                Ok(vec![PrReview {
                    author_login: Some("cursor[bot]".to_string()),
                }]),
            )
            .with_review_threads(42, Ok(vec![]));
        let mut req = fx.req();
        req.bot_logins = &bots;

        let outcome = poll_once(&fx.deps(), &req, &mut Vec::new()).unwrap();

        assert!(matches!(
            outcome,
            TickOutcome::Finished(PollOutcome::Handled)
        ));
        let run = fx.store.run_by_id(fx.run_id).unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Done);
        let events = fx.store.events_for_run(fx.run_id).unwrap();
        let done = events.iter().find(|e| e.kind == "bots_done").unwrap();
        assert_eq!(
            done.detail.as_deref(),
            Some(r#"{"total":0,"unresolved":0}"#)
        );
    }

    fn finding_detail(is_resolved: bool, author: &str, body: &str) -> FindingDetail {
        FindingDetail {
            author_login: Some(author.to_string()),
            is_resolved,
            path: Some("src/lib.rs".to_string()),
            line: Some(42),
            body: body.to_string(),
            url: "https://github.com/example/repo/pull/42#comment-1".to_string(),
        }
    }

    #[test]
    fn poll_once_bots_done_with_findings_notify_writes_findings_file_and_sets_review() {
        let bots = cursor_bot();
        let mut fx = Fixture::new();
        fx.cfg.on_bots_done = OnBotsDone::Notify;
        fx.gh = fx
            .gh
            .with_pr_state(42, Ok(PrLifecycle::Open))
            .with_pr_reviews(
                42,
                Ok(vec![PrReview {
                    author_login: Some("cursor[bot]".to_string()),
                }]),
            )
            .with_review_threads(
                42,
                Ok(vec![crate::github::bot_findings::ReviewThread {
                    is_resolved: false,
                    author_login: Some("cursor".to_string()),
                }]),
            )
            .with_pr_bot_finding_details(42, Ok(vec![finding_detail(false, "cursor", "fix this")]));
        let home_dir = tempdir().unwrap();
        let mut req = fx.req();
        req.bot_logins = &bots;
        req.home = home_dir.path();

        let outcome = poll_once(&fx.deps(), &req, &mut Vec::new()).unwrap();

        assert!(matches!(
            outcome,
            TickOutcome::Finished(PollOutcome::Handled)
        ));
        let run = fx.store.run_by_id(fx.run_id).unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Review);
        assert!(
            fx.cleanup.calls().is_empty(),
            "notify mode must not launch cleanup"
        );

        let path = findings_file_path(home_dir.path(), None, "PROJ-1");
        let contents = std::fs::read_to_string(&path).unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&contents).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["file"], "src/lib.rs");
        assert_eq!(parsed[0]["line"], 42);
        assert_eq!(parsed[0]["body"], "fix this");
        assert_eq!(
            parsed[0]["url"],
            "https://github.com/example/repo/pull/42#comment-1"
        );
    }

    #[test]
    fn poll_once_bots_done_with_findings_launch_invokes_cleanup_launcher() {
        let bots = cursor_bot();
        let mut fx = Fixture::new();
        fx.cfg.on_bots_done = OnBotsDone::Launch;
        fx.gh = fx
            .gh
            .with_pr_state(42, Ok(PrLifecycle::Open))
            .with_pr_reviews(
                42,
                Ok(vec![PrReview {
                    author_login: Some("cursor[bot]".to_string()),
                }]),
            )
            .with_review_threads(
                42,
                Ok(vec![crate::github::bot_findings::ReviewThread {
                    is_resolved: false,
                    author_login: Some("cursor".to_string()),
                }]),
            )
            .with_pr_bot_finding_details(42, Ok(vec![finding_detail(false, "cursor", "fix this")]));
        let home_dir = tempdir().unwrap();
        let mut req = fx.req();
        req.bot_logins = &bots;
        req.home = home_dir.path();

        poll_once(&fx.deps(), &req, &mut Vec::new()).unwrap();

        assert_eq!(fx.cleanup.calls(), vec!["PROJ-1".to_string()]);
    }

    // --- run_poll_loop: backoff / give-up ---

    #[test]
    fn run_poll_loop_gives_up_after_ten_consecutive_gh_failures() {
        let mut fx = Fixture::new();
        fx.gh = fx.gh.with_pr_state(
            42,
            Err(GhError::Command {
                command: "gh pr view".to_string(),
                exit_code: Some(1),
                stderr: "boom".to_string(),
            }),
        );

        let outcome = run_poll_loop(&fx.deps(), &fx.req(), &mut Vec::new());

        assert_eq!(outcome, PollOutcome::Failed);
        let run = fx.store.run_by_id(fx.run_id).unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Failed);
        let events = fx.store.events_for_run(fx.run_id).unwrap();
        let error_events = events.iter().filter(|e| e.kind == "poll_error").count();
        assert_eq!(error_events, 10);
    }

    #[test]
    fn run_poll_loop_wall_clock_timeout_gives_up_without_calling_gh() {
        let mut fx = Fixture::new();
        fx.cfg.max_wait_mins = 10;
        fx.clock = FakeClock::at(1_000 + 10 * 60 + 1);

        let outcome = run_poll_loop(&fx.deps(), &fx.req(), &mut Vec::new());

        assert_eq!(outcome, PollOutcome::GaveUp);
        let run = fx.store.run_by_id(fx.run_id).unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Failed);
        let events = fx.store.events_for_run(fx.run_id).unwrap();
        assert!(events.iter().any(|e| e.kind == "give_up"));
        assert!(fx.gh.pr_state_calls().is_empty(), "must not call gh at all");
    }

    // --- findings_file_path ---

    #[test]
    fn findings_file_path_uses_xdg_data_home_when_set() {
        let home = Path::new("/home/user");
        let xdg = Path::new("/custom/data");

        let path = findings_file_path(home, Some(xdg), "PROJ-372");

        assert_eq!(
            path,
            PathBuf::from("/custom/data/tskmstr/findings/proj-372.json")
        );
    }

    #[test]
    fn findings_file_path_falls_back_to_home_local_share() {
        let home = Path::new("/home/user");

        let path = findings_file_path(home, None, "PROJ-372");

        assert_eq!(
            path,
            PathBuf::from("/home/user/.local/share/tskmstr/findings/proj-372.json")
        );
    }

    #[test]
    fn findings_file_path_lowercases_the_key() {
        let path = findings_file_path(Path::new("/home/user"), None, "ABC-9");
        assert!(path.ends_with("abc-9.json"));
    }
}
