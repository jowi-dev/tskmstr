//! The autopilot dispatcher core (GitHub issue #115): [`tick`] picks ready
//! tickets for one scope and launches lane runs for them, within the
//! memory budget, the usage ceilings, and `[dispatch] max_lanes`.
//!
//! A tick is idempotent. It holds a per-scope `flock` ([`FileScopeLock`])
//! for its whole length, so two hosts (the TUI toggle and the `tm dispatch`
//! timer) never launch the same ticket. Once a ticket is launched it has a
//! live lane run, which removes it from the next tick's candidates.
//!
//! In order, a tick:
//!
//! 1. Takes the scope lock, or returns [`Skip::Locked`].
//! 2. Reads the scope's autopilot flag from runs.db, or returns
//!    [`Skip::Disabled`]. Otherwise stamps the scope's `last_tick_at`, which
//!    `tm dispatch status` shows.
//! 3. Takes `tm ready`'s ready and stackable tickets (audit gate included)
//!    from a [`CandidateSource`], and drops every ticket the run store says
//!    is already in hand ([`in_hand`]).
//! 4. Orders them by how many other candidates each unblocks, most first,
//!    keeping the tracker's rank order within a tie. When the scope has a
//!    priority epic ([`RunStore::autopilot_epic`], GitHub issue #237), that
//!    epic's tickets come first, each group in that same order; tickets
//!    outside it stay eligible.
//! 5. Launches them in that order through a [`Launcher`] while the running
//!    lane count is under `max_lanes` (counted per scope, ADR-0011), the
//!    scope launched fewer than
//!    `max_launches_per_day` lanes in the last 24 hours, and memory and
//!    usage admission pass.
//!    The first refusal ends the tick; a failed launch is recorded and the
//!    tick moves on to the next ticket.
//! 6. Records a `launch` row in `dispatch_events` per launch, and a
//!    `refused` row, with its [`Refusal::code`], for the tick's first
//!    refusal.
//! 7. With `[dispatch] fix_failing_ci` on, runs the CI-fix sweep
//!    ([`ci_fix::sweep`], GitHub issue #226): a fix pass for each lane PR
//!    whose checks finished red on a head commit not yet handled, started
//!    detached through a [`ci_fix::CiFixLauncher`]. A fix pass is not a
//!    lane, so it counts against neither `max_lanes` nor the daily cap;
//!    `tm review ci-fix` applies admission itself.
//! 8. Sends the desktop notifications the scope owes
//!    ([`crate::work::notify`]): tickets that entered awaiting review or hit
//!    the fix-round cap, and a refusal streak older than
//!    `stall_notify_mins`. Each transition notifies once.
//!
//! Every dependency is injected, so tests drive [`tick`] with a real
//! [`RunStore`] in a temp dir and fake candidates and launchers.

use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::cli::ready::ListDecision;
use crate::config::{MemoryConfig, UsageConfig};
use crate::runs::footprint::MemoryPressure;
use crate::runs::overview::OverviewRun;
use crate::runs::{RunStatus, RunStore, RunStoreError};
use crate::work::admission::{
    AdmissionRefusal, RouteRefusal, RunnerRefusal, UsageProber, UsageRefusal, route_unattended,
};
use crate::work::auto_review::RoundVerdict;
use crate::work::ci_fix::{self, CiFixAction};
use crate::work::detach::DetachSpawner;
use crate::work::notify::{self, Notifier};

/// `dispatch_events.kind` of a launched lane.
pub const LAUNCH_EVENT: &str = "launch";
/// `dispatch_events.kind` of a tick's first refusal.
pub const REFUSED_EVENT: &str = "refused";

/// The daily launch cap's rolling window, in seconds.
const DAY_SECS: i64 = 24 * 60 * 60;

/// One `tm ready` ticket as the dispatcher reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The ticket key.
    pub key: String,
    /// Its `tm ready` decision. Only [`ListDecision::Ready`] and
    /// [`ListDecision::Stackable`] are launched.
    pub decision: ListDecision,
    /// How many other candidates it blocks, transitively.
    pub unblocks: usize,
    /// Its parent epic's key. Only filled when the tick asked for epics
    /// ([`CandidateSource::candidates`]); `None` otherwise, or when it has
    /// no parent.
    pub epic: Option<String>,
}

/// Where a tick's candidates come from: `tm ready`'s classification in
/// production, a fixed list in tests. Candidates arrive in tracker rank
/// order.
pub trait CandidateSource {
    /// The scope's candidates, or why they couldn't be read. With
    /// `with_epics`, each one's [`Candidate::epic`] is filled, whatever
    /// `[ready] inherit_epic_blockers` says; the tick asks only when the
    /// scope has a priority epic, so a scope without one pays no extra
    /// tracker call.
    fn candidates(&self, with_epics: bool) -> Result<Vec<Candidate>, String>;
}

/// Production: `tm ready`'s own classification
/// ([`crate::cli::ready::dispatch_candidates`]).
impl CandidateSource for crate::cli::ready::ReadyContext<'_> {
    fn candidates(&self, with_epics: bool) -> Result<Vec<Candidate>, String> {
        crate::cli::ready::dispatch_candidates(self, with_epics).map_err(|err| err.to_string())
    }
}

/// Starts one lane run. Production runs `tm work run`; see
/// [`CommandLauncher`].
pub trait Launcher {
    /// Launches `ticket` in `lane`, or describes why it failed. Returns
    /// once the run is recorded in the run store, so the next tick sees it
    /// as live.
    fn launch(&self, lane: &str, ticket: &str) -> Result<(), String>;
}

/// Production [`Launcher`]: runs `<program> work run --unattended <lane>
/// <ticket>` in `cwd` and waits for it. `tm work run --unattended` opens
/// the lane interactively in the ticket's `work` window (GitHub issue #184)
/// and returns once the window is launched and the lane's awaiter is
/// spawned. The run row stays `running`, so it counts toward `max_lanes`
/// and `in_hand`, until the lane writes its done marker, its session ends,
/// or it is hibernated. It also re-checks memory and usage admission, and
/// keeps metered runners out of the fallback chain.
pub struct CommandLauncher {
    /// The `tm` binary, normally `std::env::current_exe()`.
    pub program: PathBuf,
    /// Where to run it: the scope's repo, so `tm` loads its config.
    pub cwd: PathBuf,
}

impl Launcher for CommandLauncher {
    fn launch(&self, lane: &str, ticket: &str) -> Result<(), String> {
        let output = std::process::Command::new(&self.program)
            .args(["work", "run", "--unattended", lane, ticket])
            .current_dir(&self.cwd)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|err| format!("could not run {}: {err}", self.program.display()))?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("{}: {}", output.status, stderr.trim()))
    }
}

/// Production [`ci_fix::CiFixLauncher`]: spawns `<program> review ci-fix
/// --unattended <ticket>` detached in `cwd`, logging to `<log_dir>/<ticket
/// lowercased>.log`. Detached because the tick holds the scope lock and
/// `tm review ci-fix --unattended` may wait on admission.
pub struct CommandCiFixLauncher<'a> {
    /// The `tm` binary, normally `std::env::current_exe()`.
    pub program: PathBuf,
    /// Where to run it: the scope's repo, so `tm` loads its config.
    pub cwd: PathBuf,
    /// Where the logs go, normally [`ci_fix_log_dir`]; created on first
    /// launch.
    pub log_dir: PathBuf,
    /// Spawns the detached process.
    pub spawner: &'a dyn DetachSpawner,
}

impl ci_fix::CiFixLauncher for CommandCiFixLauncher<'_> {
    fn launch(&self, ticket: &str) -> Result<(), String> {
        std::fs::create_dir_all(&self.log_dir)
            .map_err(|err| format!("could not create {}: {err}", self.log_dir.display()))?;
        let log_path = self.log_dir.join(format!("{}.log", ticket.to_lowercase()));
        let argv = ["review", "ci-fix", "--unattended", ticket].map(str::to_string);
        self.spawner
            .spawn_detached(&self.program, &argv, &self.cwd, &log_path)
            .map(|_| ())
            .map_err(|err| err.to_string())
    }
}

/// Where [`CommandCiFixLauncher`] logs: `~/.local/state/tskmstr/ci-fix`,
/// beside the auto-review logs
/// ([`crate::cli::review::auto_review_log_path`]).
pub fn ci_fix_log_dir(home: &Path) -> PathBuf {
    home.join(".local")
        .join("state")
        .join("tskmstr")
        .join("ci-fix")
}

/// Held for as long as a tick runs; dropping it releases the scope lock.
pub struct ScopeGuard {
    _file: Option<File>,
}

impl ScopeGuard {
    /// A guard holding `file`'s `flock`, or nothing (for a lock that needs
    /// no file).
    pub fn new(file: Option<File>) -> Self {
        ScopeGuard { _file: file }
    }
}

/// The per-scope mutual exclusion a tick runs under.
pub trait ScopeLock {
    /// Takes `scope`'s lock without waiting: `None` when another holder
    /// has it.
    fn try_lock(&self, scope: &str) -> io::Result<Option<ScopeGuard>>;
}

/// [`ScopeLock`] over an exclusive `flock` on `<dir>/<scope>.lock`. The
/// kernel drops the lock when the holder exits, so a crashed tick never
/// leaves a scope locked.
pub struct FileScopeLock {
    /// The directory holding the lock files; created on first use.
    pub dir: PathBuf,
}

impl ScopeLock for FileScopeLock {
    fn try_lock(&self, scope: &str) -> io::Result<Option<ScopeGuard>> {
        std::fs::create_dir_all(&self.dir)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.dir.join(lock_file_name(scope)))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(ScopeGuard::new(Some(file)))),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(err)) => Err(err),
        }
    }
}

/// The lock file name for `scope`. A scope such as `github:o/r` holds
/// characters a file name can't, so everything but ASCII letters, digits,
/// `-` and `.` becomes `_`.
fn lock_file_name(scope: &str) -> String {
    let safe: String = scope
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{safe}.lock")
}

/// Where [`FileScopeLock`] keeps its files: `$XDG_RUNTIME_DIR/tskmstr/dispatch`
/// when that is set, since it is per-user and cleared at logout, or
/// `<data_dir>/dispatch` (next to runs.db) otherwise.
pub fn default_lock_dir(xdg_runtime_dir: Option<&Path>, data_dir: &Path) -> PathBuf {
    match xdg_runtime_dir {
        Some(runtime) => runtime.join("tskmstr").join("dispatch"),
        None => data_dir.join("dispatch"),
    }
}

/// A lane a tick may launch into, from the board's lane resolution
/// ([`crate::config::compatible_lane_names`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneChoice {
    /// The `[work.lanes.<name>]` name.
    pub name: String,
    /// Its repo key for memory admission
    /// ([`crate::work::run::lane_repo_key`]).
    pub repo: String,
}

/// What a tick reads from config.
pub struct TickConfig<'a> {
    /// The scope ([`crate::config::BackendIdentity::scope`]) to dispatch.
    pub scope: &'a str,
    /// `[dispatch] max_lanes`.
    pub max_lanes: usize,
    /// `[dispatch] max_launches_per_day`: the most launches in any rolling
    /// 24 hours for this scope.
    pub max_launches_per_day: usize,
    /// `[dispatch] max_fix_rounds`, for fix-cap notifications and the
    /// CI-fix sweep's cap.
    pub max_fix_rounds: u32,
    /// `[dispatch] fix_failing_ci`: run the CI-fix sweep (GitHub issue
    /// #226).
    pub fix_failing_ci: bool,
    /// `[dispatch] stall_notify_mins`: how long refusals for one reason
    /// last before a stall notification.
    pub stall_notify_mins: u64,
    /// The lanes compatible with this scope, in board order. The board
    /// launches directly when there is one and asks when there are more; a
    /// tick can't ask, so it takes the first.
    pub lanes: &'a [LaneChoice],
    /// The runners an unattended launch may use, in fallback order:
    /// [`crate::agent::routing::unattended_order`]. Admission walks them and
    /// launches on the first that passes (GitHub issue #192). Empty when
    /// every runner is metered and paid fallback is off.
    pub runners: &'a [&'a str],
    /// `[work] memory_budget_gb` and friends.
    pub memory: &'a MemoryConfig,
    /// `[agent.<runner>]` usage ceilings.
    pub usage: &'a UsageConfig,
}

/// What a tick calls out to.
pub struct TickDeps<'a> {
    /// The run store: autopilot flag, runs, dispatch events.
    pub store: &'a RunStore,
    /// The per-scope lock.
    pub lock: &'a dyn ScopeLock,
    /// `tm ready`'s candidates.
    pub candidates: &'a dyn CandidateSource,
    /// Starts lane runs.
    pub launcher: &'a dyn Launcher,
    /// Reads kernel memory pressure for memory admission.
    pub memory_pressure: &'a dyn Fn() -> MemoryPressure,
    /// Refreshes a stale usage snapshot for usage admission.
    pub usage_prober: &'a dyn UsageProber,
    /// Unix seconds, for usage admission, the daily launch cap and stall
    /// notifications.
    pub now: i64,
    /// Shows the tick's notifications ([`crate::work::notify`]).
    pub notifier: &'a dyn Notifier,
    /// What the CI-fix sweep reads PR checks with and launches through.
    /// `None` runs no sweep, whatever [`TickConfig::fix_failing_ci`] says.
    pub ci: Option<&'a ci_fix::SweepDeps<'a>>,
}

/// Why a tick did nothing at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// Another host holds the scope lock.
    Locked,
    /// Autopilot is off for the scope.
    Disabled,
}

/// Why a tick stopped launching (or, for [`Refusal::LaunchFailed`], skipped
/// one ticket).
#[derive(Debug, Clone, PartialEq)]
pub enum Refusal {
    /// `max_lanes` lanes are already running in the scope.
    MaxLanes {
        /// Tickets of the lanes counted: the scope's running lanes plus
        /// legacy unscoped ones, oldest first (see
        /// [`RunStore::running_lane_tickets`]).
        running: Vec<String>,
        /// `[dispatch] max_lanes`.
        max: usize,
    },
    /// The scope launched `[dispatch] max_launches_per_day` lanes in the
    /// last 24 hours.
    DailyCap {
        /// Launches recorded for the scope in the last 24 hours.
        launched: usize,
        /// `[dispatch] max_launches_per_day`.
        max: usize,
    },
    /// No configured lane is compatible with the scope.
    NoLane,
    /// Every runner is metered and `[dispatch] allow_paid_fallback` is off.
    NoUnattendedRunner,
    /// Memory admission refused.
    Memory(AdmissionRefusal),
    /// Usage admission refused.
    Usage(UsageRefusal),
    /// With more than one runner, every runner refused (GitHub issue
    /// #192); each entry is a runner and its reason.
    EveryRunner(RouteRefusal),
    /// The launcher failed for one ticket.
    LaunchFailed {
        /// The ticket that failed to launch.
        ticket: String,
        /// The launcher's error.
        error: String,
    },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::MaxLanes { running, max } => write!(
                f,
                "not launching: {} lanes running ({}), [dispatch] max_lanes is {max}",
                running.len(),
                running.join(", ")
            ),
            Refusal::DailyCap { launched, max } => write!(
                f,
                "not launching: {launched} lanes launched in the last 24h, [dispatch] max_launches_per_day is {max}"
            ),
            Refusal::NoLane => write!(f, "not launching: no configured lane matches this repo"),
            Refusal::NoUnattendedRunner => write!(
                f,
                "not launching: every runner is metered and [dispatch] allow_paid_fallback is off"
            ),
            Refusal::Memory(refusal) => refusal.fmt(f),
            Refusal::Usage(refusal) => refusal.fmt(f),
            Refusal::EveryRunner(refusal) => refusal.fmt(f),
            Refusal::LaunchFailed { ticket, error } => {
                write!(f, "launching {ticket} failed: {error}")
            }
        }
    }
}

impl Refusal {
    /// A stable code for the refusal's kind, recorded in
    /// `dispatch_events.reason`. Two refusals with one code are "the same
    /// reason" for stall notifications, whatever their detail says.
    pub fn code(&self) -> &'static str {
        match self {
            Refusal::MaxLanes { .. } => "max_lanes",
            Refusal::DailyCap { .. } => "daily_cap",
            Refusal::NoLane => "no_lane",
            Refusal::NoUnattendedRunner => "no_unattended_runner",
            Refusal::Memory(_) => "memory",
            Refusal::Usage(_) => "usage",
            Refusal::EveryRunner(refusal) => every_runner_code(refusal),
            Refusal::LaunchFailed { .. } => "launch_failed",
        }
    }
}

/// [`Refusal::code`] of [`Refusal::EveryRunner`]: `memory` or `usage` when
/// every runner refused for that one kind of reason, so a streak of them is
/// one stall, or `every_runner` when the reasons are mixed.
fn every_runner_code(refusal: &RouteRefusal) -> &'static str {
    let code = |r: &RunnerRefusal| match r {
        RunnerRefusal::Memory(_) => "memory",
        RunnerRefusal::Usage(_) => "usage",
    };
    let mut codes = refusal.0.iter().map(|(_, r)| code(r));
    let first = codes.next().unwrap_or("every_runner");
    if codes.all(|c| c == first) {
        first
    } else {
        "every_runner"
    }
}

/// One lane a tick launched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    /// The ticket.
    pub ticket: String,
    /// The lane it launched in.
    pub lane: String,
    /// The runner admission picked for it (GitHub issue #192). `tm work run
    /// --unattended` walks the same runners and normally lands on the same
    /// one; this is for display.
    pub runner: String,
    /// The branch it stacks on, for a stackable ticket. `tm work run`
    /// resolves the stacking itself; this is for display.
    pub stacked_on: Option<String>,
}

/// What one [`tick`] did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TickReport {
    /// The lanes launched, in launch order.
    pub launched: Vec<Launch>,
    /// The first refusal, if the tick stopped short or a launch failed.
    pub refused: Option<Refusal>,
    /// Set when the tick did nothing at all.
    pub skipped: Option<Skip>,
    /// The titles of the notifications the tick sent, in order.
    pub notified: Vec<String>,
    /// What the CI-fix sweep did, one entry per ticket it acted on.
    pub ci_fixes: Vec<CiFixAction>,
}

/// Errors that stop a tick outright.
#[derive(Debug, Error)]
pub enum DispatchError {
    /// The run store couldn't be read or written.
    #[error(transparent)]
    Store(#[from] RunStoreError),
    /// The scope lock couldn't be taken for a reason other than another
    /// holder.
    #[error("could not take the dispatch lock: {0}")]
    Lock(#[from] io::Error),
    /// The candidates couldn't be read.
    #[error("could not read ready tickets: {0}")]
    Candidates(String),
}

/// Runs one dispatch tick for `cfg.scope`; see the module doc for the
/// steps.
///
/// # Errors
///
/// Returns a [`DispatchError`] when the lock, the run store, or the
/// candidate source fails. Admission refusals and launch failures are not
/// errors; they land in [`TickReport::refused`].
pub fn tick(deps: &TickDeps, cfg: &TickConfig) -> Result<TickReport, DispatchError> {
    let Some(_guard) = deps.lock.try_lock(cfg.scope)? else {
        return Ok(skipped(Skip::Locked));
    };
    if !deps.store.autopilot_enabled(cfg.scope)? {
        return Ok(skipped(Skip::Disabled));
    }
    deps.store.stamp_autopilot_tick(cfg.scope)?;
    let priority_epic = deps.store.autopilot_epic(cfg.scope)?;

    let runs = deps.store.overview_runs()?;
    let busy = in_hand(&runs, cfg.scope);
    let mut queue: Vec<Candidate> = deps
        .candidates
        .candidates(priority_epic.is_some())
        .map_err(DispatchError::Candidates)?
        .into_iter()
        .filter(|c| {
            matches!(
                c.decision,
                ListDecision::Ready | ListDecision::Stackable { .. }
            )
        })
        .filter(|c| !busy.contains_key(&c.key))
        .collect();
    // A stable sort: rank order survives within each (epic, unblocks) tie.
    queue.sort_by_key(|c| {
        let outside_epic = priority_epic.is_none() || c.epic != priority_epic;
        (outside_epic, std::cmp::Reverse(c.unblocks))
    });

    let mut report = TickReport::default();
    let mut refused_ticket = None;
    let mut running = deps.store.running_lane_tickets(cfg.scope)?;
    for candidate in queue {
        match admit(deps, cfg, &running)? {
            Ok((lane, runner)) => match deps.launcher.launch(&lane.name, &candidate.key) {
                Ok(()) => {
                    let stacked_on = match candidate.decision {
                        ListDecision::Stackable { base, .. } => Some(base),
                        _ => None,
                    };
                    let detail = match &stacked_on {
                        Some(base) => format!("lane {} on {runner} stacked on {base}", lane.name),
                        None => format!("lane {} on {runner}", lane.name),
                    };
                    deps.store.record_dispatch_event(
                        cfg.scope,
                        Some(&candidate.key),
                        LAUNCH_EVENT,
                        &detail,
                    )?;
                    running.push(candidate.key.clone());
                    report.launched.push(Launch {
                        ticket: candidate.key,
                        lane: lane.name.clone(),
                        runner: runner.to_string(),
                        stacked_on,
                    });
                }
                Err(error) => {
                    if report.refused.is_none() {
                        refused_ticket = Some(candidate.key.clone());
                        report.refused = Some(Refusal::LaunchFailed {
                            ticket: candidate.key,
                            error,
                        });
                    }
                }
            },
            Err(refusal) => {
                if report.refused.is_none() {
                    refused_ticket = Some(candidate.key);
                    report.refused = Some(refusal);
                }
                break;
            }
        }
    }

    if let Some(refusal) = &report.refused {
        deps.store.record_dispatch_refusal(
            cfg.scope,
            refused_ticket.as_deref(),
            refusal.code(),
            &refusal.to_string(),
        )?;
    }

    if let Some(ci) = deps.ci.filter(|_| cfg.fix_failing_ci) {
        report.ci_fixes = ci_fix::sweep(ci, &runs, cfg.scope, cfg.max_fix_rounds)?;
    }

    let mut notices = notify::ticket_notices(&runs, cfg.scope, cfg.max_fix_rounds);
    notices.extend(notify::stall_notice(
        cfg.scope,
        deps.store.refusal_streak(cfg.scope)?.as_ref(),
        deps.now,
        i64::try_from(cfg.stall_notify_mins.saturating_mul(60)).unwrap_or(i64::MAX),
    ));
    report.notified = notify::send(deps.store, deps.notifier, cfg.scope, &notices)?;
    Ok(report)
}

fn skipped(skip: Skip) -> TickReport {
    TickReport {
        skipped: Some(skip),
        ..TickReport::default()
    }
}

/// Whether one more lane may launch now, with the scope's `running` lanes:
/// the lane to launch into and the runner to launch it on, or the first
/// gate that refuses. Gates run cheapest first, so a full machine never
/// spawns a usage probe. Memory and usage admission run per runner
/// ([`route_unattended`]), so an over-ceiling runner falls through to the
/// next.
fn admit<'c>(
    deps: &TickDeps,
    cfg: &'c TickConfig,
    running: &[String],
) -> Result<Result<(&'c LaneChoice, &'c str), Refusal>, DispatchError> {
    if running.len() >= cfg.max_lanes {
        return Ok(Err(Refusal::MaxLanes {
            running: running.to_vec(),
            max: cfg.max_lanes,
        }));
    }
    let launched = deps
        .store
        .dispatch_launches_since(cfg.scope, deps.now - DAY_SECS)?;
    if launched >= cfg.max_launches_per_day {
        return Ok(Err(Refusal::DailyCap {
            launched,
            max: cfg.max_launches_per_day,
        }));
    }
    let Some(lane) = cfg.lanes.first() else {
        return Ok(Err(Refusal::NoLane));
    };
    if cfg.runners.is_empty() {
        return Ok(Err(Refusal::NoUnattendedRunner));
    }
    let routed = route_unattended(
        deps.store,
        cfg.memory,
        cfg.usage,
        cfg.runners,
        Some(&lane.repo),
        deps.memory_pressure,
        deps.now,
        deps.usage_prober,
    )?;
    Ok(match routed {
        Ok(routed) => Ok((lane, cfg.runners[routed.index])),
        Err(mut refusal) if refusal.0.len() == 1 => Err(match refusal.0.remove(0).1 {
            RunnerRefusal::Memory(refusal) => Refusal::Memory(refusal),
            RunnerRefusal::Usage(refusal) => Refusal::Usage(refusal),
        }),
        Err(refusal) => Err(Refusal::EveryRunner(refusal)),
    })
}

/// What a scope's dispatcher is doing, for `tm dispatch status` and the
/// TUI autopilot header (GitHub issue #121).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DispatchState {
    /// When the scope last launched a lane, in unix seconds.
    pub last_launch_unix: Option<i64>,
    /// What is stopping launches now: set while the scope's newest dispatch
    /// event is a refusal.
    pub blocked: Option<Blocked>,
}

/// The refusal a scope's dispatcher is stuck on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocked {
    /// When refusals for this reason began, in unix seconds: the start of
    /// the [`RunStore::refusal_streak`], or the refusal itself when it has
    /// no reason code.
    pub since_unix: i64,
    /// The newest refusal's detail.
    pub detail: String,
}

/// Reads `scope`'s [`DispatchState`] from `dispatch_events`.
///
/// # Errors
///
/// Returns the store's error when a read fails.
pub fn dispatch_state(store: &RunStore, scope: &str) -> Result<DispatchState, RunStoreError> {
    let last_launch_unix = store
        .last_dispatch_event(scope, LAUNCH_EVENT)?
        .map(|event| event.created_unix);
    let blocked = match store.dispatch_events(scope, 1)?.into_iter().next() {
        Some(newest) if newest.kind == REFUSED_EVENT => {
            let since_unix = store
                .refusal_streak(scope)?
                .map_or(newest.created_unix, |streak| streak.since_unix);
            Some(Blocked {
                since_unix,
                detail: newest.detail,
            })
        }
        _ => None,
    };
    Ok(DispatchState {
        last_launch_unix,
        blocked,
    })
}

/// Why the run store says a ticket is already in hand, so a tick leaves it
/// alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InHand {
    /// A run of any kind is live (running, queued, or hibernated).
    Live,
    /// A lane run was launched for it before. A tick launches a ticket at
    /// most once: a lane that ended without a PR (failed, interrupted, or
    /// done) is a human's call, and retrying it unattended could loop.
    Attempted,
    /// A run recorded a PR and no run recorded it merged or closed.
    OpenPr,
    /// Its newest auto-review round hit the round cap
    /// ([`RoundVerdict::Capped`]).
    FixRoundCap,
}

/// The tickets in `scope` that the run store says are already in hand,
/// from [`RunStore::overview_runs`] (newest first). Legacy unscoped runs
/// (`scope = ''`) count for every scope, as they do elsewhere.
pub fn in_hand(runs: &[OverviewRun], scope: &str) -> HashMap<String, InHand> {
    let mut by_ticket: HashMap<&str, Vec<&OverviewRun>> = HashMap::new();
    for run in runs
        .iter()
        .filter(|run| run.scope == scope || run.scope.is_empty())
    {
        by_ticket.entry(&run.ticket).or_default().push(run);
    }

    let capped = format!(" {}", RoundVerdict::Capped.as_str());
    by_ticket
        .into_iter()
        .filter_map(|(ticket, runs)| {
            let reason = if runs.iter().any(|run| {
                matches!(
                    run.status,
                    RunStatus::Running | RunStatus::Queued | RunStatus::Hibernated
                )
            }) {
                InHand::Live
            } else if runs.iter().any(|run| run.kind == "lane") {
                InHand::Attempted
            } else if runs.iter().any(|run| run.pr_url.is_some())
                && !runs.iter().any(|run| run.pr_closed)
            {
                InHand::OpenPr
            } else if runs
                .iter()
                .find_map(|run| run.auto_review_round.as_deref())
                .is_some_and(|detail| detail.ends_with(&capped))
            {
                InHand::FixRoundCap
            } else {
                return None;
            };
            Some((ticket.to_string(), reason))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use super::*;
    use crate::agent::UsageSnapshot;
    use crate::runs::footprint::GIB;
    use crate::runs::{FinishRun, StartRun, UsageSnapshotRow};
    use crate::work::auto_review::ROUND_EVENT;
    use tempfile::{TempDir, tempdir};

    /// Records every notification a tick sends.
    #[derive(Default)]
    struct Recorder(RefCell<Vec<String>>);

    impl Notifier for Recorder {
        fn notify(&self, title: &str, _body: &str) {
            self.0.borrow_mut().push(title.to_string());
        }
    }

    const SCOPE: &str = "github:o/r";
    const NOW: i64 = 1_000_000;

    /// Candidates handed back verbatim, recording whether the tick asked
    /// for their parent epics.
    struct FakeCandidates(Vec<Candidate>, Cell<Option<bool>>);

    impl CandidateSource for FakeCandidates {
        fn candidates(&self, with_epics: bool) -> Result<Vec<Candidate>, String> {
            self.1.set(Some(with_epics));
            Ok(self.0.clone())
        }
    }

    /// Records each launch and, like `tm work run`, starts a running lane
    /// row for it. Tickets in `fail` fail instead.
    struct FakeLauncher<'a> {
        store: &'a RunStore,
        fail: Vec<&'static str>,
        launched: RefCell<Vec<(String, String)>>,
    }

    impl<'a> FakeLauncher<'a> {
        fn new(store: &'a RunStore) -> Self {
            FakeLauncher {
                store,
                fail: Vec::new(),
                launched: RefCell::new(Vec::new()),
            }
        }

        fn tickets(&self) -> Vec<String> {
            self.launched
                .borrow()
                .iter()
                .map(|(_, t)| t.clone())
                .collect()
        }
    }

    impl Launcher for FakeLauncher<'_> {
        fn launch(&self, lane: &str, ticket: &str) -> Result<(), String> {
            if self.fail.contains(&ticket) {
                return Err("worktree is dirty".to_string());
            }
            start(self.store, ticket, "lane");
            self.launched
                .borrow_mut()
                .push((lane.to_string(), ticket.to_string()));
            Ok(())
        }
    }

    /// A prober that is never consulted.
    struct NoProbe;

    impl UsageProber for NoProbe {
        fn supports(&self, _agent: &str) -> bool {
            false
        }

        fn probe(&self, _agent: &str) -> Result<UsageSnapshot, String> {
            unreachable!("no probe expected")
        }
    }

    struct Fixture {
        dir: TempDir,
        store: RunStore,
        lanes: Vec<LaneChoice>,
        memory: MemoryConfig,
        usage: UsageConfig,
        max_lanes: usize,
        runners: Vec<&'static str>,
        pressure: MemoryPressure,
        max_launches_per_day: usize,
        now: i64,
        max_fix_rounds: u32,
        stall_notify_mins: u64,
        fix_failing_ci: bool,
        notifier: Recorder,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempdir().unwrap();
            let store = RunStore::open(&dir.path().join("runs.db")).unwrap();
            store.set_autopilot(SCOPE, true).unwrap();
            Fixture {
                dir,
                store,
                lanes: vec![LaneChoice {
                    name: "backend".to_string(),
                    repo: "r".to_string(),
                }],
                memory: MemoryConfig::default(),
                usage: UsageConfig::default(),
                max_lanes: 5,
                runners: vec!["claude"],
                pressure: MemoryPressure::Normal,
                max_launches_per_day: 20,
                now: NOW,
                max_fix_rounds: 3,
                stall_notify_mins: 30,
                fix_failing_ci: false,
                notifier: Recorder::default(),
            }
        }

        fn lock(&self) -> FileScopeLock {
            FileScopeLock {
                dir: self.dir.path().join("locks"),
            }
        }

        fn tick(&self, candidates: &[Candidate], launcher: &dyn Launcher) -> TickReport {
            self.tick_asking(candidates, launcher).0
        }

        /// [`Fixture::tick`], also returning whether the tick asked its
        /// candidate source for parent epics (`None` when it never asked).
        fn tick_asking(
            &self,
            candidates: &[Candidate],
            launcher: &dyn Launcher,
        ) -> (TickReport, Option<bool>) {
            self.tick_with_ci_asking(candidates, launcher, None)
        }

        /// [`Fixture::tick`] with `ci` as the tick's CI-fix sweep deps.
        fn tick_with_ci(
            &self,
            candidates: &[Candidate],
            launcher: &dyn Launcher,
            ci: Option<&ci_fix::SweepDeps<'_>>,
        ) -> TickReport {
            self.tick_with_ci_asking(candidates, launcher, ci).0
        }

        /// [`Fixture::tick_with_ci`], also returning whether the tick asked
        /// its candidate source for parent epics.
        fn tick_with_ci_asking(
            &self,
            candidates: &[Candidate],
            launcher: &dyn Launcher,
            ci: Option<&ci_fix::SweepDeps<'_>>,
        ) -> (TickReport, Option<bool>) {
            let lock = self.lock();
            let source = FakeCandidates(candidates.to_vec(), Cell::new(None));
            let pressure = self.pressure;
            let pressure_fn = move || pressure;
            let deps = TickDeps {
                store: &self.store,
                lock: &lock,
                candidates: &source,
                launcher,
                memory_pressure: &pressure_fn,
                usage_prober: &NoProbe,
                now: self.now,
                notifier: &self.notifier,
                ci,
            };
            let cfg = TickConfig {
                scope: SCOPE,
                max_lanes: self.max_lanes,
                max_launches_per_day: self.max_launches_per_day,
                max_fix_rounds: self.max_fix_rounds,
                stall_notify_mins: self.stall_notify_mins,
                fix_failing_ci: self.fix_failing_ci,
                lanes: &self.lanes,
                runners: &self.runners,
                memory: &self.memory,
                usage: &self.usage,
            };
            let report = tick(&deps, &cfg).unwrap();
            (report, source.1.get())
        }

        fn events(&self) -> Vec<(Option<String>, String, String)> {
            self.store
                .dispatch_events(SCOPE, 100)
                .unwrap()
                .into_iter()
                .rev()
                .map(|e| (e.ticket, e.kind, e.detail))
                .collect()
        }
    }

    fn ready(key: &str, unblocks: usize) -> Candidate {
        Candidate {
            key: key.to_string(),
            decision: ListDecision::Ready,
            unblocks,
            epic: None,
        }
    }

    /// A ready candidate whose parent epic is `epic`.
    fn ready_in(key: &str, unblocks: usize, epic: &str) -> Candidate {
        Candidate {
            epic: Some(epic.to_string()),
            ..ready(key, unblocks)
        }
    }

    fn start(store: &RunStore, ticket: &str, kind: &str) -> i64 {
        store
            .start_run(&StartRun {
                ticket: ticket.to_string(),
                scope: SCOPE.to_string(),
                lane: "backend".to_string(),
                worktree: format!("/wt/{ticket}"),
                branch: None,
                pid: None,
                kind: kind.to_string(),
                log_path: None,
            })
            .unwrap()
    }

    fn finish(store: &RunStore, run_id: i64, status: RunStatus, pr_url: Option<&str>) {
        store
            .finish_run(
                run_id,
                &FinishRun {
                    status,
                    exit_code: None,
                    session_id: None,
                    cost_usd: None,
                    num_turns: None,
                    blocker: None,
                    pr_url: pr_url.map(str::to_string),
                    transcript: None,
                    model_usage: None,
                    findings_count: None,
                },
            )
            .unwrap();
    }

    // --- CI-fix sweep (GitHub issue #226) ---

    /// Records each ticket a CI fix was started for.
    #[derive(Default)]
    struct FakeCiFixLauncher(RefCell<Vec<String>>);

    impl ci_fix::CiFixLauncher for FakeCiFixLauncher {
        fn launch(&self, ticket: &str) -> Result<(), String> {
            self.0.borrow_mut().push(ticket.to_string());
            Ok(())
        }
    }

    /// `gh` reporting PR 7 open and red on head `abc1234`.
    fn red_pr_gh() -> crate::github::gh_cli::FakeGhCli {
        use crate::github::gh_cli::{CheckResult, ChecksState, PrChecks, PrLifecycle};
        crate::github::gh_cli::FakeGhCli::new().with_pr_checks(
            7,
            Ok(PrChecks {
                lifecycle: PrLifecycle::Open,
                head_sha: "abc1234".to_string(),
                checks: vec![CheckResult {
                    name: "test".to_string(),
                    state: ChecksState::Failing,
                    run_id: Some(2),
                    details_url: None,
                    conclusion: Some("FAILURE".to_string()),
                }],
            }),
        )
    }

    /// A finished lane run for `ticket` whose PR is number 7.
    fn finished_lane_with_pr(store: &RunStore, ticket: &str) {
        let lane = start(store, ticket, "lane");
        finish(
            store,
            lane,
            RunStatus::Done,
            Some("https://github.com/o/r/pull/7"),
        );
    }

    #[test]
    fn with_fix_failing_ci_on_a_red_lane_pr_gets_a_ci_fix() {
        let mut fx = Fixture::new();
        fx.fix_failing_ci = true;
        finished_lane_with_pr(&fx.store, "GH-1");
        let gh = red_pr_gh();
        let ci_launcher = FakeCiFixLauncher::default();
        let ci = ci_fix::SweepDeps {
            store: &fx.store,
            gh: &gh,
            repo_dir: Path::new("/repo"),
            launcher: &ci_launcher,
        };
        let launcher = FakeLauncher::new(&fx.store);

        let report = fx.tick_with_ci(&[ready("GH-2", 0)], &launcher, Some(&ci));

        assert_eq!(*ci_launcher.0.borrow(), vec!["GH-1"]);
        assert_eq!(
            report.ci_fixes,
            vec![ci_fix::CiFixAction::Started {
                ticket: "GH-1".to_string(),
                sha: "abc1234".to_string(),
            }]
        );
        assert_eq!(launcher.tickets(), vec!["GH-2"], "lanes still launch");
    }

    #[test]
    fn with_fix_failing_ci_off_checks_are_never_read() {
        let fx = Fixture::new();
        finished_lane_with_pr(&fx.store, "GH-1");
        let gh = red_pr_gh();
        let ci_launcher = FakeCiFixLauncher::default();
        let ci = ci_fix::SweepDeps {
            store: &fx.store,
            gh: &gh,
            repo_dir: Path::new("/repo"),
            launcher: &ci_launcher,
        };
        let launcher = FakeLauncher::new(&fx.store);

        let report = fx.tick_with_ci(&[], &launcher, Some(&ci));

        assert!(gh.pr_checks_calls().is_empty());
        assert!(ci_launcher.0.borrow().is_empty());
        assert!(report.ci_fixes.is_empty());
    }

    #[test]
    fn a_ci_fix_does_not_count_against_max_lanes() {
        let mut fx = Fixture::new();
        fx.fix_failing_ci = true;
        fx.max_lanes = 0;
        finished_lane_with_pr(&fx.store, "GH-1");
        let gh = red_pr_gh();
        let ci_launcher = FakeCiFixLauncher::default();
        let ci = ci_fix::SweepDeps {
            store: &fx.store,
            gh: &gh,
            repo_dir: Path::new("/repo"),
            launcher: &ci_launcher,
        };
        let launcher = FakeLauncher::new(&fx.store);

        let report = fx.tick_with_ci(&[ready("GH-2", 0)], &launcher, Some(&ci));

        assert!(matches!(report.refused, Some(Refusal::MaxLanes { .. })));
        assert_eq!(*ci_launcher.0.borrow(), vec!["GH-1"]);
    }

    #[test]
    fn command_ci_fix_launcher_spawns_tm_review_ci_fix_detached() {
        let dir = tempdir().unwrap();
        let spawner = crate::work::detach::FakeDetachSpawner::new(4242);
        let log_dir = dir.path().join("state/ci-fix");
        let launcher = CommandCiFixLauncher {
            program: PathBuf::from("/usr/local/bin/tm"),
            cwd: PathBuf::from("/repo"),
            log_dir: log_dir.clone(),
            spawner: &spawner,
        };

        ci_fix::CiFixLauncher::launch(&launcher, "GH-12").unwrap();

        let recorded = spawner.recorded.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].program, PathBuf::from("/usr/local/bin/tm"));
        assert_eq!(
            recorded[0].argv,
            vec!["review", "ci-fix", "--unattended", "GH-12"]
        );
        assert_eq!(recorded[0].working_dir, PathBuf::from("/repo"));
        assert_eq!(recorded[0].log_path, log_dir.join("gh-12.log"));
        assert!(log_dir.is_dir(), "the log dir is created");
    }

    #[test]
    fn ci_fix_log_dir_sits_beside_the_auto_review_logs() {
        let home = Path::new("/home/me");
        assert_eq!(
            ci_fix_log_dir(home),
            PathBuf::from("/home/me/.local/state/tskmstr/ci-fix")
        );
        assert_eq!(
            crate::cli::review::auto_review_log_path(home, "GH-1").parent(),
            Some(Path::new("/home/me/.local/state/tskmstr/auto-review"))
        );
    }

    #[test]
    fn launches_by_unblock_count_then_rank_order() {
        let fx = Fixture::new();
        let launcher = FakeLauncher::new(&fx.store);

        let report = fx.tick(
            &[ready("GH-1", 0), ready("GH-2", 2), ready("GH-3", 0)],
            &launcher,
        );

        assert_eq!(launcher.tickets(), vec!["GH-2", "GH-1", "GH-3"]);
        assert_eq!(report.launched.len(), 3);
        assert_eq!(report.refused, None);
        assert_eq!(report.skipped, None);
    }

    // --- priority epic (GitHub issue #237) ---

    #[test]
    fn the_priority_epics_tickets_launch_first() {
        let mut fx = Fixture::new();
        fx.max_lanes = 1;
        fx.store.set_autopilot_epic(SCOPE, Some("GH-100")).unwrap();
        let launcher = FakeLauncher::new(&fx.store);

        fx.tick(
            &[ready("GH-1", 3), ready_in("GH-2", 0, "GH-100")],
            &launcher,
        );

        assert_eq!(launcher.tickets(), vec!["GH-2"]);
    }

    #[test]
    fn each_group_keeps_unblock_then_rank_order() {
        let fx = Fixture::new();
        fx.store.set_autopilot_epic(SCOPE, Some("GH-100")).unwrap();
        let launcher = FakeLauncher::new(&fx.store);

        fx.tick(
            &[
                ready("GH-1", 0),
                ready_in("GH-2", 0, "GH-100"),
                ready("GH-3", 2),
                ready_in("GH-4", 1, "GH-100"),
                ready_in("GH-5", 0, "GH-200"),
            ],
            &launcher,
        );

        assert_eq!(
            launcher.tickets(),
            vec!["GH-4", "GH-2", "GH-3", "GH-1", "GH-5"]
        );
    }

    #[test]
    fn with_no_ready_epic_ticket_the_rest_launch_as_before() {
        let fx = Fixture::new();
        fx.store.set_autopilot_epic(SCOPE, Some("GH-100")).unwrap();
        let launcher = FakeLauncher::new(&fx.store);

        fx.tick(
            &[ready("GH-1", 0), ready("GH-2", 2), ready("GH-3", 0)],
            &launcher,
        );

        assert_eq!(launcher.tickets(), vec!["GH-2", "GH-1", "GH-3"]);
    }

    #[test]
    fn parent_epics_are_fetched_only_when_an_epic_is_set() {
        let fx = Fixture::new();
        let launcher = FakeLauncher::new(&fx.store);
        let (_, asked) = fx.tick_asking(&[], &launcher);
        assert_eq!(asked, Some(false));

        fx.store.set_autopilot_epic(SCOPE, Some("GH-100")).unwrap();
        let (_, asked) = fx.tick_asking(&[], &launcher);
        assert_eq!(asked, Some(true));
    }

    #[test]
    fn launches_into_the_first_compatible_lane() {
        let mut fx = Fixture::new();
        fx.lanes.push(LaneChoice {
            name: "frontend".to_string(),
            repo: "f".to_string(),
        });
        let launcher = FakeLauncher::new(&fx.store);

        fx.tick(&[ready("GH-1", 0)], &launcher);

        assert_eq!(
            *launcher.launched.borrow(),
            vec![("backend".to_string(), "GH-1".to_string())]
        );
    }

    #[test]
    fn a_held_scope_lock_skips_the_tick() {
        let fx = Fixture::new();
        let launcher = FakeLauncher::new(&fx.store);
        let _held = fx.lock().try_lock(SCOPE).unwrap().expect("first lock");

        let report = fx.tick(&[ready("GH-1", 0)], &launcher);

        assert_eq!(report.skipped, Some(Skip::Locked));
        assert!(launcher.tickets().is_empty());
    }

    #[test]
    fn the_scope_lock_is_released_when_the_tick_ends() {
        let fx = Fixture::new();
        let launcher = FakeLauncher::new(&fx.store);

        fx.tick(&[], &launcher);

        assert!(
            take_lock(&fx).is_some(),
            "the scope lock is still held after the tick"
        );
    }

    /// Takes the fixture's scope lock, retrying for up to a second.
    ///
    /// Another test thread that forks a child while a tick's lock file is
    /// open hands the child a copy of the descriptor, and with it the
    /// `flock`, until the child execs and close-on-exec drops it. So retry
    /// briefly instead of failing on that window.
    fn take_lock(fx: &Fixture) -> Option<ScopeGuard> {
        (0..100).find_map(|_| {
            let guard = fx.lock().try_lock(SCOPE).unwrap();
            if guard.is_none() {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            guard
        })
    }

    #[test]
    fn scopes_lock_independently() {
        let fx = Fixture::new();
        let _held = fx.lock().try_lock("github:o/other").unwrap().unwrap();

        assert!(fx.lock().try_lock(SCOPE).unwrap().is_some());
    }

    #[test]
    fn a_disabled_scope_skips_the_tick() {
        let fx = Fixture::new();
        fx.store.set_autopilot(SCOPE, false).unwrap();
        let launcher = FakeLauncher::new(&fx.store);

        let report = fx.tick(&[ready("GH-1", 0)], &launcher);

        assert_eq!(report.skipped, Some(Skip::Disabled));
        assert!(launcher.tickets().is_empty());
        assert!(fx.events().is_empty());
    }

    /// GH-9 opened a PR and its watch finished clean: awaiting review.
    fn awaiting_review(store: &RunStore) {
        let lane = start(store, "GH-9", "lane");
        finish(store, lane, RunStatus::Done, Some("https://gh/pr/9"));
        let watch = start(store, "GH-9", "review-watch");
        finish(store, watch, RunStatus::Done, None);
    }

    #[test]
    fn awaiting_review_notifies_once_across_ticks() {
        let fx = Fixture::new();
        awaiting_review(&fx.store);
        let launcher = FakeLauncher::new(&fx.store);

        let first = fx.tick(&[], &launcher);
        let second = fx.tick(&[], &launcher);

        assert_eq!(first.notified, vec!["tm: GH-9 is awaiting your review"]);
        assert!(second.notified.is_empty());
        assert_eq!(*fx.notifier.0.borrow(), first.notified);
    }

    #[test]
    fn a_disabled_scope_sends_no_notifications() {
        let fx = Fixture::new();
        fx.store.set_autopilot(SCOPE, false).unwrap();
        awaiting_review(&fx.store);
        let launcher = FakeLauncher::new(&fx.store);

        fx.tick(&[], &launcher);

        assert!(fx.notifier.0.borrow().is_empty());
    }

    #[test]
    fn a_stall_notifies_once_after_stall_notify_mins() {
        let mut fx = Fixture::new();
        fx.max_lanes = 0;
        fx.now = unix_now();
        let launcher = FakeLauncher::new(&fx.store);

        let first = fx.tick(&[ready("GH-1", 0)], &launcher);
        fx.now += 31 * 60;
        let late = fx.tick(&[ready("GH-1", 0)], &launcher);
        let later = fx.tick(&[ready("GH-1", 0)], &launcher);

        assert!(first.notified.is_empty(), "not stalled long enough yet");
        assert_eq!(late.notified.len(), 1);
        assert!(
            late.notified[0].starts_with("tm autopilot stalled for 31 min"),
            "{:?}",
            late.notified
        );
        assert!(later.notified.is_empty(), "one stall notifies once");
    }

    fn last_tick_at(store: &RunStore) -> Option<String> {
        store
            .autopilot_scopes()
            .unwrap()
            .into_iter()
            .find(|s| s.scope == SCOPE)
            .and_then(|s| s.last_tick_at)
    }

    #[test]
    fn an_enabled_tick_stamps_the_scope_even_with_nothing_to_launch() {
        let fx = Fixture::new();
        let launcher = FakeLauncher::new(&fx.store);

        fx.tick(&[], &launcher);

        assert!(last_tick_at(&fx.store).is_some());
    }

    #[test]
    fn a_skipped_tick_leaves_the_last_tick_alone() {
        let fx = Fixture::new();
        let launcher = FakeLauncher::new(&fx.store);
        fx.store.set_autopilot(SCOPE, false).unwrap();
        fx.tick(&[], &launcher);
        let _held = take_lock(&fx).unwrap();
        fx.store.set_autopilot(SCOPE, true).unwrap();
        fx.tick(&[], &launcher);

        assert!(last_tick_at(&fx.store).is_none());
    }

    #[test]
    fn two_ticks_in_a_row_never_double_launch() {
        let fx = Fixture::new();
        let launcher = FakeLauncher::new(&fx.store);
        let candidates = [ready("GH-1", 0), ready("GH-2", 0)];

        fx.tick(&candidates, &launcher);
        let second = fx.tick(&candidates, &launcher);

        assert_eq!(launcher.tickets(), vec!["GH-1", "GH-2"]);
        assert!(second.launched.is_empty());
        assert_eq!(second.refused, None);
    }

    #[test]
    fn stackable_tickets_launch_and_waiting_or_blocked_ones_are_skipped() {
        let fx = Fixture::new();
        let launcher = FakeLauncher::new(&fx.store);
        let stackable = Candidate {
            key: "GH-2".to_string(),
            decision: ListDecision::Stackable {
                base: "origin/gh-1-x".to_string(),
                blocker_key: "GH-1".to_string(),
                pr_number: 7,
            },
            unblocks: 0,
            epic: None,
        };
        let waiting = Candidate {
            key: "GH-3".to_string(),
            decision: ListDecision::Waiting {
                blocker_key: "GH-9".to_string(),
                run_id: 1,
                run_status: "running".to_string(),
                reason: "waiting on GH-9 (running)".to_string(),
            },
            unblocks: 5,
            epic: None,
        };
        let blocked = Candidate {
            key: "GH-4".to_string(),
            decision: ListDecision::Blocked {
                blocker_keys: vec!["GH-8".to_string()],
                reason: "blocked".to_string(),
            },
            unblocks: 5,
            epic: None,
        };
        let needs_audit = Candidate {
            key: "GH-5".to_string(),
            decision: ListDecision::NeedsAudit {
                reason: "not audited".to_string(),
            },
            unblocks: 5,
            epic: None,
        };

        let report = fx.tick(&[stackable, waiting, blocked, needs_audit], &launcher);

        assert_eq!(launcher.tickets(), vec!["GH-2"]);
        assert_eq!(
            report.launched[0].stacked_on.as_deref(),
            Some("origin/gh-1-x")
        );
        assert_eq!(
            fx.events(),
            vec![(
                Some("GH-2".to_string()),
                LAUNCH_EVENT.to_string(),
                "lane backend on claude stacked on origin/gh-1-x".to_string()
            )]
        );
    }

    #[test]
    fn max_lanes_counts_running_lanes_and_records_the_refusal() {
        let mut fx = Fixture::new();
        fx.max_lanes = 2;
        start(&fx.store, "GH-OTHER", "lane");
        let launcher = FakeLauncher::new(&fx.store);

        let report = fx.tick(&[ready("GH-1", 0), ready("GH-2", 0)], &launcher);

        assert_eq!(launcher.tickets(), vec!["GH-1"]);
        assert_eq!(
            report.refused,
            Some(Refusal::MaxLanes {
                running: vec!["GH-OTHER".to_string(), "GH-1".to_string()],
                max: 2
            })
        );
        assert_eq!(
            fx.events().last().unwrap(),
            &(
                Some("GH-2".to_string()),
                REFUSED_EVENT.to_string(),
                "not launching: 2 lanes running (GH-OTHER, GH-1), [dispatch] max_lanes is 2"
                    .to_string()
            )
        );
    }

    #[test]
    fn max_lanes_counts_only_the_ticking_scopes_lanes() {
        let mut fx = Fixture::new();
        fx.max_lanes = 1;
        fx.store
            .start_run(&StartRun {
                ticket: "AX-809".to_string(),
                scope: "jira:AX".to_string(),
                lane: "backend".to_string(),
                worktree: "/wt/ax-809".to_string(),
                branch: None,
                pid: None,
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();
        let launcher = FakeLauncher::new(&fx.store);

        let report = fx.tick(&[ready("GH-1", 0)], &launcher);

        assert_eq!(launcher.tickets(), vec!["GH-1"]);
        assert_eq!(report.refused, None);
    }

    #[test]
    fn a_refusal_event_carries_its_reason_code() {
        let mut fx = Fixture::new();
        fx.max_lanes = 0;
        let launcher = FakeLauncher::new(&fx.store);

        fx.tick(&[ready("GH-1", 0)], &launcher);

        let event = fx.store.dispatch_events(SCOPE, 1).unwrap().remove(0);
        assert_eq!(event.reason.as_deref(), Some("max_lanes"));
    }

    #[test]
    fn the_daily_launch_cap_refuses_with_its_own_reason() {
        let mut fx = Fixture::new();
        fx.max_launches_per_day = 2;
        fx.now = unix_now();
        let launcher = FakeLauncher::new(&fx.store);

        let report = fx.tick(
            &[ready("GH-1", 0), ready("GH-2", 0), ready("GH-3", 0)],
            &launcher,
        );

        assert_eq!(launcher.tickets(), vec!["GH-1", "GH-2"]);
        assert_eq!(
            report.refused,
            Some(Refusal::DailyCap {
                launched: 2,
                max: 2
            })
        );
        let event = fx.store.dispatch_events(SCOPE, 1).unwrap().remove(0);
        assert_eq!(event.reason.as_deref(), Some("daily_cap"));
        assert_eq!(
            event.detail,
            "not launching: 2 lanes launched in the last 24h, [dispatch] max_launches_per_day is 2"
        );
    }

    #[test]
    fn launches_older_than_a_day_leave_the_daily_cap() {
        let mut fx = Fixture::new();
        fx.max_launches_per_day = 1;
        fx.now = unix_now();
        let launcher = FakeLauncher::new(&fx.store);
        fx.tick(&[ready("GH-1", 0)], &launcher);

        fx.now += 24 * 60 * 60 + 60;
        let report = fx.tick(&[ready("GH-2", 0)], &launcher);

        assert_eq!(launcher.tickets(), vec!["GH-1", "GH-2"]);
        assert_eq!(report.refused, None);
    }

    fn unix_now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    #[test]
    fn nothing_to_launch_records_no_refusal() {
        let mut fx = Fixture::new();
        fx.max_lanes = 0;
        let launcher = FakeLauncher::new(&fx.store);

        let report = fx.tick(&[], &launcher);

        assert_eq!(report, TickReport::default());
        assert!(fx.events().is_empty());
    }

    #[test]
    fn no_compatible_lane_refuses() {
        let mut fx = Fixture::new();
        fx.lanes.clear();
        let launcher = FakeLauncher::new(&fx.store);

        let report = fx.tick(&[ready("GH-1", 0)], &launcher);

        assert_eq!(report.refused, Some(Refusal::NoLane));
        assert!(launcher.tickets().is_empty());
    }

    #[test]
    fn no_unattended_runner_refuses() {
        let mut fx = Fixture::new();
        fx.runners = Vec::new();
        let launcher = FakeLauncher::new(&fx.store);

        let report = fx.tick(&[ready("GH-1", 0)], &launcher);

        assert_eq!(report.refused, Some(Refusal::NoUnattendedRunner));
    }

    #[test]
    fn memory_pressure_refuses() {
        let mut fx = Fixture::new();
        fx.memory.budget_bytes = Some(64 * GIB);
        fx.pressure = MemoryPressure::Critical;
        let launcher = FakeLauncher::new(&fx.store);

        let report = fx.tick(&[ready("GH-1", 0)], &launcher);

        assert_eq!(
            report.refused,
            Some(Refusal::Memory(AdmissionRefusal::Pressure(
                MemoryPressure::Critical
            )))
        );
        assert!(launcher.tickets().is_empty());
    }

    #[test]
    fn the_memory_budget_refuses_once_running_lanes_fill_it() {
        let mut fx = Fixture::new();
        fx.memory.budget_bytes = Some(3 * GIB);
        fx.memory.default_lane_estimate_bytes = 2 * GIB;
        let launcher = FakeLauncher::new(&fx.store);

        let report = fx.tick(&[ready("GH-1", 0), ready("GH-2", 0)], &launcher);

        assert_eq!(launcher.tickets(), vec!["GH-1"]);
        assert!(matches!(
            report.refused,
            Some(Refusal::Memory(AdmissionRefusal::OverBudget(_)))
        ));
    }

    #[test]
    fn an_exhausted_usage_window_refuses() {
        let fx = Fixture::new();
        fx.store
            .set_agent_exhausted_until("claude", NOW + 600)
            .unwrap();
        let launcher = FakeLauncher::new(&fx.store);

        let report = fx.tick(&[ready("GH-1", 0)], &launcher);

        assert!(matches!(
            report.refused,
            Some(Refusal::Usage(UsageRefusal::Exhausted { .. }))
        ));
        assert!(launcher.tickets().is_empty());
    }

    #[test]
    fn a_usage_window_over_its_ceiling_refuses() {
        let fx = Fixture::new();
        fx.store
            .upsert_usage_snapshot(&UsageSnapshotRow {
                agent: "claude".to_string(),
                window: "five_hour".to_string(),
                utilization: 0.95,
                resets_at: NOW + 3600,
                observed_at: NOW - 10,
                source: "stream".to_string(),
                status: Some("allowed".to_string()),
            })
            .unwrap();
        let launcher = FakeLauncher::new(&fx.store);

        let report = fx.tick(&[ready("GH-1", 0)], &launcher);

        assert!(matches!(
            report.refused,
            Some(Refusal::Usage(UsageRefusal::OverCeiling { .. }))
        ));
    }

    fn over_ceiling(store: &RunStore, agent: &str, utilization: f64) {
        store
            .upsert_usage_snapshot(&UsageSnapshotRow {
                agent: agent.to_string(),
                window: "five_hour".to_string(),
                utilization,
                resets_at: NOW + 3600,
                observed_at: NOW - 10,
                source: "stream".to_string(),
                status: Some("allowed".to_string()),
            })
            .unwrap();
    }

    #[test]
    fn an_over_ceiling_runner_falls_through_to_the_next_one() {
        let mut fx = Fixture::new();
        fx.runners = vec!["claude", "opencode"];
        over_ceiling(&fx.store, "claude", 0.85);
        let launcher = FakeLauncher::new(&fx.store);

        let report = fx.tick(&[ready("GH-1", 0)], &launcher);

        assert_eq!(report.refused, None);
        assert_eq!(launcher.tickets(), vec!["GH-1"]);
        assert_eq!(report.launched[0].runner, "opencode");
        let (_, kind, detail) = fx.events().pop().unwrap();
        assert_eq!(kind, LAUNCH_EVENT);
        assert!(detail.contains("opencode"), "{detail}");
    }

    #[test]
    fn a_tick_every_runner_refuses_names_each_reason() {
        let mut fx = Fixture::new();
        fx.runners = vec!["claude", "opencode"];
        over_ceiling(&fx.store, "claude", 0.85);
        fx.usage.opencode.token_budget = Some(crate::config::TokenBudget {
            tokens: 0,
            window_secs: 3600,
        });
        let launcher = FakeLauncher::new(&fx.store);

        let report = fx.tick(&[ready("GH-1", 0)], &launcher);

        assert!(launcher.tickets().is_empty());
        let Some(Refusal::EveryRunner(refusal)) = &report.refused else {
            panic!("expected every runner to refuse: {:?}", report.refused);
        };
        let runners: Vec<&str> = refusal.0.iter().map(|(r, _)| r.as_str()).collect();
        assert_eq!(runners, vec!["claude", "opencode"]);
        assert_eq!(report.refused.as_ref().unwrap().code(), "usage");
        let (_, kind, detail) = fx.events().pop().unwrap();
        assert_eq!(kind, REFUSED_EVENT);
        assert!(detail.contains("claude 5h usage at 85%"), "{detail}");
        assert!(detail.contains("opencode used 0 tokens"), "{detail}");
    }

    #[test]
    fn a_token_budget_refuses_a_signal_less_runner() {
        let mut fx = Fixture::new();
        fx.runners = vec!["opencode"];
        fx.usage.opencode.token_budget = Some(crate::config::TokenBudget {
            tokens: 0,
            window_secs: 3600,
        });
        let launcher = FakeLauncher::new(&fx.store);

        let report = fx.tick(&[ready("GH-1", 0)], &launcher);

        assert!(matches!(
            report.refused,
            Some(Refusal::Usage(UsageRefusal::OverTokenBudget { .. }))
        ));
    }

    #[test]
    fn a_failed_launch_is_recorded_and_the_tick_moves_on() {
        let fx = Fixture::new();
        let mut launcher = FakeLauncher::new(&fx.store);
        launcher.fail = vec!["GH-1"];

        let report = fx.tick(&[ready("GH-1", 0), ready("GH-2", 0)], &launcher);

        assert_eq!(launcher.tickets(), vec!["GH-2"]);
        assert_eq!(
            report.refused,
            Some(Refusal::LaunchFailed {
                ticket: "GH-1".to_string(),
                error: "worktree is dirty".to_string(),
            })
        );
        assert_eq!(
            fx.events(),
            vec![
                (
                    Some("GH-2".to_string()),
                    LAUNCH_EVENT.to_string(),
                    "lane backend on claude".to_string()
                ),
                (
                    Some("GH-1".to_string()),
                    REFUSED_EVENT.to_string(),
                    "launching GH-1 failed: worktree is dirty".to_string()
                ),
            ]
        );
    }

    #[test]
    fn dispatch_state_reads_the_last_launch_and_the_current_block() {
        let fx = Fixture::new();
        assert_eq!(
            dispatch_state(&fx.store, SCOPE).unwrap(),
            DispatchState::default()
        );

        fx.store
            .record_dispatch_event(SCOPE, Some("GH-1"), LAUNCH_EVENT, "lane backend")
            .unwrap();
        let state = dispatch_state(&fx.store, SCOPE).unwrap();
        let launched_at = state.last_launch_unix.expect("a launch");
        assert!(launched_at > NOW, "real wall-clock time");
        assert_eq!(state.blocked, None, "the newest event is a launch");

        fx.store
            .record_dispatch_refusal(SCOPE, None, "max_lanes", "max lanes 1")
            .unwrap();
        fx.store
            .record_dispatch_refusal(SCOPE, None, "max_lanes", "max lanes 2")
            .unwrap();
        let state = dispatch_state(&fx.store, SCOPE).unwrap();
        assert_eq!(state.last_launch_unix, Some(launched_at));
        let blocked = state.blocked.expect("blocked");
        assert_eq!(blocked.detail, "max lanes 2");
        assert!(blocked.since_unix >= launched_at);
    }

    #[test]
    fn a_refusal_without_a_reason_code_still_blocks() {
        let fx = Fixture::new();
        fx.store
            .record_dispatch_event(SCOPE, None, REFUSED_EVENT, "old refusal")
            .unwrap();

        let blocked = dispatch_state(&fx.store, SCOPE).unwrap().blocked.unwrap();

        assert_eq!(blocked.detail, "old refusal");
    }

    #[test]
    fn tickets_in_hand_are_not_launched() {
        let fx = Fixture::new();
        // GH-1: a live audit session.
        start(&fx.store, "GH-1", "audit");
        // GH-2: a lane run that already ended.
        let lane = start(&fx.store, "GH-2", "lane");
        finish(&fx.store, lane, RunStatus::Failed, None);
        // GH-3: an open PR awaiting review.
        let watch = start(&fx.store, "GH-3", "review-watch");
        finish(
            &fx.store,
            watch,
            RunStatus::Done,
            Some("https://github.com/o/r/pull/3"),
        );
        // GH-4: an auto-review that hit the round cap, PR since closed.
        let watch = start(&fx.store, "GH-4", "review-watch");
        finish(
            &fx.store,
            watch,
            RunStatus::Done,
            Some("https://github.com/o/r/pull/4"),
        );
        fx.store.add_event(watch, "pr_closed", None).unwrap();
        let review = start(&fx.store, "GH-4", "auto-review");
        fx.store
            .add_event(review, ROUND_EVENT, Some("2/2 capped"))
            .unwrap();
        finish(&fx.store, review, RunStatus::Review, None);
        // GH-5: in another scope only.
        fx.store
            .start_run(&StartRun {
                ticket: "GH-5".to_string(),
                scope: "github:o/other".to_string(),
                lane: "x".to_string(),
                worktree: "/wt/x".to_string(),
                branch: None,
                pid: None,
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();
        let launcher = FakeLauncher::new(&fx.store);

        let candidates: Vec<Candidate> = (1..=5).map(|n| ready(&format!("GH-{n}"), 0)).collect();
        fx.tick(&candidates, &launcher);

        assert_eq!(launcher.tickets(), vec!["GH-5"]);
        let busy = in_hand(&fx.store.overview_runs().unwrap(), SCOPE);
        assert_eq!(busy.get("GH-1"), Some(&InHand::Live));
        assert_eq!(busy.get("GH-2"), Some(&InHand::Attempted));
        assert_eq!(busy.get("GH-3"), Some(&InHand::OpenPr));
        assert_eq!(busy.get("GH-4"), Some(&InHand::FixRoundCap));
    }

    /// GH-169: an interactive audit session that records its verdict and
    /// then reads the ticket again (or reads another ticket for context)
    /// must not leave the ticket in hand.
    #[test]
    fn an_interactive_audit_session_leaves_no_ticket_in_hand() {
        use crate::agent::claude::ClaudeRunner;
        use crate::runs::session::{SessionEnv, finish_session, register_session};

        let fx = Fixture::new();
        let markers = fx.dir.path().join("sessions");
        let env = SessionEnv {
            session_id: Some("sess-1".to_string()),
            agent_pid: None,
            lane_run_id: None,
            session_run_id: None,
            cwd: PathBuf::from("/wt"),
        };
        let register = |ticket: &str| {
            register_session(&fx.store, Some(SCOPE), &markers, &env, "audit", ticket).unwrap()
        };

        register("GH-1");
        register("GH-2");
        finish_session(
            &fx.store,
            &markers,
            &env,
            "audit",
            "GH-1",
            RunStatus::Done,
            &ClaudeRunner,
        )
        .unwrap();
        register("GH-1");

        let busy = in_hand(&fx.store.overview_runs().unwrap(), SCOPE);
        assert_eq!(busy.get("GH-1"), None);
        assert_eq!(busy.get("GH-2"), None);
    }

    /// A stand-in `tm` that logs its argv and cwd, then exits `code`.
    fn fake_tm(dir: &Path, code: i32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("tm");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\necho \"$PWD $*\" > \"{}\"\necho 'lane mylane is dirty' >&2\nexit {code}\n",
                dir.join("argv").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn the_command_launcher_runs_tm_work_run_unattended_and_waits() {
        let dir = tempdir().unwrap();
        let launcher = CommandLauncher {
            program: fake_tm(dir.path(), 0),
            cwd: dir.path().to_path_buf(),
        };

        launcher.launch("backend", "GH-7").unwrap();

        let logged = std::fs::read_to_string(dir.path().join("argv")).unwrap();
        assert_eq!(
            logged.trim(),
            format!(
                "{} work run --unattended backend GH-7",
                dir.path().canonicalize().unwrap().display()
            )
        );
    }

    #[test]
    fn the_command_launcher_reports_a_failed_launch_with_its_stderr() {
        let dir = tempdir().unwrap();
        let launcher = CommandLauncher {
            program: fake_tm(dir.path(), 3),
            cwd: dir.path().to_path_buf(),
        };

        let err = launcher.launch("backend", "GH-7").unwrap_err();

        assert!(err.contains("exit status: 3"), "{err}");
        assert!(err.contains("lane mylane is dirty"), "{err}");
    }

    #[test]
    fn lock_file_names_are_safe_for_any_scope() {
        assert_eq!(lock_file_name("github:o/r"), "github_o_r.lock");
        assert_eq!(
            lock_file_name("jira:https://x.atlassian.net/PROJ"),
            "jira_https___x.atlassian.net_PROJ.lock"
        );
    }

    #[test]
    fn the_lock_dir_prefers_the_runtime_dir() {
        assert_eq!(
            default_lock_dir(Some(Path::new("/run/user/1000")), Path::new("/d")),
            PathBuf::from("/run/user/1000/tskmstr/dispatch")
        );
        assert_eq!(
            default_lock_dir(None, Path::new("/d")),
            PathBuf::from("/d/dispatch")
        );
    }
}
