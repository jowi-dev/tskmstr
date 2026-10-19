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
//!    keeping the tracker's rank order within a tie.
//! 5. Launches them in that order through a [`Launcher`] while the running
//!    lane count is under `max_lanes` and memory and usage admission pass.
//!    The first refusal ends the tick; a failed launch is recorded and the
//!    tick moves on to the next ticket.
//! 6. Records a `launch` row in `dispatch_events` per launch, and a
//!    `refused` row for the tick's first refusal.
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
    AdmissionRefusal, UsageProber, UsageRefusal, UsageVerdict, check_lane_admission,
    check_usage_admission,
};
use crate::work::auto_review::RoundVerdict;

/// `dispatch_events.kind` of a launched lane.
pub const LAUNCH_EVENT: &str = "launch";
/// `dispatch_events.kind` of a tick's first refusal.
pub const REFUSED_EVENT: &str = "refused";

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
}

/// Where a tick's candidates come from: `tm ready`'s classification in
/// production, a fixed list in tests. Candidates arrive in tracker rank
/// order.
pub trait CandidateSource {
    /// The scope's candidates, or why they couldn't be read.
    fn candidates(&self) -> Result<Vec<Candidate>, String>;
}

/// Production: `tm ready`'s own classification
/// ([`crate::cli::ready::dispatch_candidates`]).
impl CandidateSource for crate::cli::ready::ReadyContext<'_> {
    fn candidates(&self) -> Result<Vec<Candidate>, String> {
        crate::cli::ready::dispatch_candidates(self).map_err(|err| err.to_string())
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
/// <ticket>` in `cwd` and waits for it. `tm work run --unattended` is
/// headless, so it returns once it has started the run row and handed the
/// agent to a detached supervisor. It also re-checks memory and usage
/// admission, and keeps metered runners out of the fallback chain.
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
    /// The lanes compatible with this scope, in board order. The board
    /// launches directly when there is one and asks when there are more; a
    /// tick can't ask, so it takes the first.
    pub lanes: &'a [LaneChoice],
    /// The runner an unattended launch prefers: the first of
    /// [`crate::agent::routing::unattended_order`], or `None` when every
    /// runner is metered and paid fallback is off.
    pub agent: Option<&'a str>,
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
    /// Unix seconds, for usage admission.
    pub now: i64,
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
    /// `max_lanes` lanes are already running.
    MaxLanes {
        /// Lanes running now, across every scope.
        running: usize,
        /// `[dispatch] max_lanes`.
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
                "not launching: {running} lanes running, [dispatch] max_lanes is {max}"
            ),
            Refusal::NoLane => write!(f, "not launching: no configured lane matches this repo"),
            Refusal::NoUnattendedRunner => write!(
                f,
                "not launching: every runner is metered and [dispatch] allow_paid_fallback is off"
            ),
            Refusal::Memory(refusal) => refusal.fmt(f),
            Refusal::Usage(refusal) => refusal.fmt(f),
            Refusal::LaunchFailed { ticket, error } => {
                write!(f, "launching {ticket} failed: {error}")
            }
        }
    }
}

/// One lane a tick launched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    /// The ticket.
    pub ticket: String,
    /// The lane it launched in.
    pub lane: String,
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

    let busy = in_hand(&deps.store.overview_runs()?, cfg.scope);
    let mut queue: Vec<Candidate> = deps
        .candidates
        .candidates()
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
    queue.sort_by_key(|c| std::cmp::Reverse(c.unblocks));

    let mut report = TickReport::default();
    let mut refused_ticket = None;
    let mut running = deps.store.running_lanes()?.len();
    for candidate in queue {
        match admit(deps, cfg, running)? {
            Ok(lane) => match deps.launcher.launch(&lane.name, &candidate.key) {
                Ok(()) => {
                    let stacked_on = match candidate.decision {
                        ListDecision::Stackable { base, .. } => Some(base),
                        _ => None,
                    };
                    let detail = match &stacked_on {
                        Some(base) => format!("lane {} stacked on {base}", lane.name),
                        None => format!("lane {}", lane.name),
                    };
                    deps.store.record_dispatch_event(
                        cfg.scope,
                        Some(&candidate.key),
                        LAUNCH_EVENT,
                        &detail,
                    )?;
                    report.launched.push(Launch {
                        ticket: candidate.key,
                        lane: lane.name.clone(),
                        stacked_on,
                    });
                    running += 1;
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
        deps.store.record_dispatch_event(
            cfg.scope,
            refused_ticket.as_deref(),
            REFUSED_EVENT,
            &refusal.to_string(),
        )?;
    }
    Ok(report)
}

fn skipped(skip: Skip) -> TickReport {
    TickReport {
        skipped: Some(skip),
        ..TickReport::default()
    }
}

/// Whether one more lane may launch now, with `running` lanes running:
/// the lane to launch into, or the first gate that refuses. Gates run
/// cheapest first, so a full machine never spawns a usage probe.
fn admit<'c>(
    deps: &TickDeps,
    cfg: &'c TickConfig,
    running: usize,
) -> Result<Result<&'c LaneChoice, Refusal>, DispatchError> {
    if running >= cfg.max_lanes {
        return Ok(Err(Refusal::MaxLanes {
            running,
            max: cfg.max_lanes,
        }));
    }
    let Some(lane) = cfg.lanes.first() else {
        return Ok(Err(Refusal::NoLane));
    };
    let Some(agent) = cfg.agent else {
        return Ok(Err(Refusal::NoUnattendedRunner));
    };
    if let Some(refusal) = check_lane_admission(
        deps.store,
        cfg.memory,
        agent,
        &lane.repo,
        deps.memory_pressure,
    )? {
        return Ok(Err(Refusal::Memory(refusal)));
    }
    if let Some(usage) = cfg.usage.for_agent(agent)
        && let UsageVerdict::Refuse(refusal) =
            check_usage_admission(deps.store, agent, usage, deps.now, deps.usage_prober)?
    {
        return Ok(Err(Refusal::Usage(refusal)));
    }
    Ok(Ok(lane))
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
    use std::cell::RefCell;

    use super::*;
    use crate::agent::UsageSnapshot;
    use crate::runs::footprint::GIB;
    use crate::runs::{FinishRun, StartRun, UsageSnapshotRow};
    use crate::work::auto_review::ROUND_EVENT;
    use tempfile::{TempDir, tempdir};

    const SCOPE: &str = "github:o/r";
    const NOW: i64 = 1_000_000;

    /// Candidates handed back verbatim.
    struct FakeCandidates(Vec<Candidate>);

    impl CandidateSource for FakeCandidates {
        fn candidates(&self) -> Result<Vec<Candidate>, String> {
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
        agent: Option<&'static str>,
        pressure: MemoryPressure,
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
                agent: Some("claude"),
                pressure: MemoryPressure::Normal,
            }
        }

        fn lock(&self) -> FileScopeLock {
            FileScopeLock {
                dir: self.dir.path().join("locks"),
            }
        }

        fn tick(&self, candidates: &[Candidate], launcher: &dyn Launcher) -> TickReport {
            let lock = self.lock();
            let source = FakeCandidates(candidates.to_vec());
            let pressure = self.pressure;
            let pressure_fn = move || pressure;
            let deps = TickDeps {
                store: &self.store,
                lock: &lock,
                candidates: &source,
                launcher,
                memory_pressure: &pressure_fn,
                usage_prober: &NoProbe,
                now: NOW,
            };
            let cfg = TickConfig {
                scope: SCOPE,
                max_lanes: self.max_lanes,
                lanes: &self.lanes,
                agent: self.agent,
                memory: &self.memory,
                usage: &self.usage,
            };
            tick(&deps, &cfg).unwrap()
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

        // Another test thread that forks a child while the tick's lock file
        // is open hands the child a copy of the descriptor, and with it the
        // `flock`, until the child execs and close-on-exec drops it. So
        // retry briefly instead of failing on that window.
        let released = (0..100).any(|_| {
            let taken = fx.lock().try_lock(SCOPE).unwrap().is_some();
            if !taken {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            taken
        });
        assert!(released, "the scope lock is still held after the tick");
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
        let _held = fx.lock().try_lock(SCOPE).unwrap().unwrap();
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
        };
        let blocked = Candidate {
            key: "GH-4".to_string(),
            decision: ListDecision::Blocked {
                blocker_keys: vec!["GH-8".to_string()],
                reason: "blocked".to_string(),
            },
            unblocks: 5,
        };
        let needs_audit = Candidate {
            key: "GH-5".to_string(),
            decision: ListDecision::NeedsAudit {
                reason: "not audited".to_string(),
            },
            unblocks: 5,
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
                "lane backend stacked on origin/gh-1-x".to_string()
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
            Some(Refusal::MaxLanes { running: 2, max: 2 })
        );
        assert_eq!(
            fx.events().last().unwrap(),
            &(
                Some("GH-2".to_string()),
                REFUSED_EVENT.to_string(),
                "not launching: 2 lanes running, [dispatch] max_lanes is 2".to_string()
            )
        );
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
        fx.agent = None;
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

    #[test]
    fn a_token_budget_refuses_a_signal_less_runner() {
        let mut fx = Fixture::new();
        fx.agent = Some("opencode");
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
                    "lane backend".to_string()
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
