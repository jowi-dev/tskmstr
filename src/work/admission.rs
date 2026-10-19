//! Memory-budget admission for lane launches (GitHub issue #66).
//!
//! Before a lane launches, tm estimates its cost as the recent peak
//! footprint for the same agent and repo (see
//! [`RunStore::lane_peak_estimate`]), falling back to
//! [`MemoryConfig::default_lane_estimate_bytes`] with no history. The launch
//! is admitted when that estimate plus the estimates of every running lane
//! fits [`MemoryConfig::budget_bytes`], and the kernel is not reporting warn
//! or critical memory pressure.
//!
//! A running lane is counted at the larger of its own peak so far and its
//! key's estimate, because a lane that is cheap at launch can peak minutes
//! later when it builds.
//!
//! Admission is off unless a budget is configured. Off means no check at
//! all, the pressure gate included, so launches behave as they did before
//! this module existed.
//!
//! Usage admission (GitHub issue #114), [`check_usage_admission`], sits
//! beside it. It refuses a launch while the agent's subscription usage
//! windows are at their configured ceilings, as read from runs.db's
//! `usage_snapshots`, and probes for a fresh value through a
//! [`UsageProber`] when the snapshot is stale. [`apply_usage_gate`] decides
//! whether a refusal only warns (a manual `tm work run`) or blocks
//! (unattended dispatch). See `docs/plans/gh-113-proactive-usage-signal.md`
//! and ADR-0008's GH-114 amendment.

use std::collections::HashMap;
use std::fmt;
use std::io::Write;

use crate::agent::{AgentInvocation, AgentRunner, UsageSnapshot};
use crate::config::{MemoryConfig, RunnerUsageConfig};
use crate::runs::footprint::{MemoryPressure, format_bytes};
use crate::runs::{RunStore, RunStoreError};
use crate::work::runner::{ProcessSpawner, SpawnRequest};

/// Why a lane launch was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmissionRefusal {
    /// The kernel reports warn or critical memory pressure.
    Pressure(MemoryPressure),
    /// The new lane's estimate plus the running lanes' would exceed the
    /// budget.
    OverBudget(OverBudget),
}

/// The numbers behind an [`AdmissionRefusal::OverBudget`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverBudget {
    /// The new lane's agent.
    pub agent: String,
    /// The new lane's repo.
    pub repo: String,
    /// The new lane's estimated peak.
    pub estimate_bytes: u64,
    /// Whether `estimate_bytes` came from recorded runs (`true`) or the
    /// configured default (`false`).
    pub from_history: bool,
    /// How many lanes are running.
    pub running_count: usize,
    /// The running lanes' summed estimates.
    pub running_bytes: u64,
    /// The configured budget.
    pub budget_bytes: u64,
}

impl fmt::Display for AdmissionRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AdmissionRefusal::Pressure(level) => write!(
                f,
                "not launching: the kernel reports {} memory pressure",
                level.as_str()
            ),
            AdmissionRefusal::OverBudget(o) => {
                let estimate = if o.from_history {
                    format!(
                        "{}/{} lanes peak ~{}",
                        o.repo,
                        o.agent,
                        format_bytes(o.estimate_bytes)
                    )
                } else {
                    format!(
                        "{}/{} lanes have no recorded peak, assuming ~{}",
                        o.repo,
                        o.agent,
                        format_bytes(o.estimate_bytes)
                    )
                };
                write!(
                    f,
                    "not launching: memory budget exceeded: {estimate}; {} running (~{}); budget {}",
                    o.running_count,
                    format_bytes(o.running_bytes),
                    format_bytes(o.budget_bytes)
                )
            }
        }
    }
}

impl std::error::Error for AdmissionRefusal {}

/// Decides whether a lane for `agent` against `repo` may launch now.
/// Returns `None` to admit it, or the reason it was refused.
///
/// `pressure` is only read when a budget is configured, so a machine with
/// admission off never pays for the sysctl.
///
/// # Errors
///
/// Returns a [`RunStoreError`] if the run store can't be read.
pub fn check_lane_admission(
    store: &RunStore,
    memory: &MemoryConfig,
    agent: &str,
    repo: &str,
    pressure: &dyn Fn() -> MemoryPressure,
) -> Result<Option<AdmissionRefusal>, RunStoreError> {
    let Some(budget_bytes) = memory.budget_bytes else {
        return Ok(None);
    };

    let level = pressure();
    if level.blocks_launch() {
        return Ok(Some(AdmissionRefusal::Pressure(level)));
    }

    let running = running_lane_estimate(store, memory)?;
    let history = store.lane_peak_estimate(agent, repo)?;
    let estimate_bytes = history.unwrap_or(memory.default_lane_estimate_bytes);
    if running.bytes + estimate_bytes <= budget_bytes {
        return Ok(None);
    }

    Ok(Some(AdmissionRefusal::OverBudget(OverBudget {
        agent: agent.to_string(),
        repo: repo.to_string(),
        estimate_bytes,
        from_history: history.is_some(),
        running_count: running.count,
        running_bytes: running.bytes,
        budget_bytes,
    })))
}

/// What the running lanes count against the memory budget; see
/// [`running_lane_estimate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunningEstimate {
    /// How many lanes are running.
    pub count: usize,
    /// Their summed estimates, in bytes.
    pub bytes: u64,
}

/// Sums the running lanes' estimated cost the way admission does: each
/// lane at the larger of its own peak so far and its `(agent, repo)` key's
/// estimate, falling back to [`MemoryConfig::default_lane_estimate_bytes`]
/// with no history. Shared with the overview's system header (GitHub issue
/// #86), which shows it against the budget.
///
/// # Errors
///
/// Returns a [`RunStoreError`] if the run store can't be read.
pub fn running_lane_estimate(
    store: &RunStore,
    memory: &MemoryConfig,
) -> Result<RunningEstimate, RunStoreError> {
    let mut estimates: HashMap<(String, String), Option<u64>> = HashMap::new();
    let running = store.running_lanes()?;
    let mut bytes = 0u64;
    for lane in &running {
        let key_estimate = match (&lane.agent, &lane.repo) {
            (Some(agent), Some(repo)) => {
                let key = (agent.clone(), repo.clone());
                match estimates.get(&key) {
                    Some(cached) => *cached,
                    None => {
                        let estimate = store.lane_peak_estimate(agent, repo)?;
                        estimates.insert(key, estimate);
                        estimate
                    }
                }
            }
            _ => None,
        };
        let estimate = key_estimate.unwrap_or(memory.default_lane_estimate_bytes);
        bytes += estimate.max(lane.mem_peak_bytes.unwrap_or(0));
    }
    Ok(RunningEstimate {
        count: running.len(),
        bytes,
    })
}

/// Reads an agent's current subscription usage on demand, for
/// [`check_usage_admission`] when the stored snapshot is stale (GitHub
/// issue #114). A trait so tests can count probes without spawning
/// anything; production uses [`SpawnUsageProber`].
pub trait UsageProber {
    /// Whether `agent` has a probe at all. A runner with no usage signal
    /// returns `false` and is never probed.
    fn supports(&self, agent: &str) -> bool;

    /// Probes `agent`'s usage, or describes why the probe failed. A
    /// snapshot with no windows means the agent reports no usage signal
    /// (claude under an API key, for example).
    fn probe(&self, agent: &str) -> Result<UsageSnapshot, String>;
}

/// Production [`UsageProber`]: runs the agent's
/// [`AgentRunner::usage_probe_invocation`] (one cheap turn) in an empty
/// temporary directory, so it loads no project context, then reads the
/// usage event out of its output with the runner's own
/// [`AgentRunner::parse_outcome`]. The invocation carries the same
/// billing-safety env stripping as a lane run.
pub struct SpawnUsageProber<'a> {
    /// The runners that may be probed, looked up by [`AgentRunner::name`].
    pub runners: Vec<&'a dyn AgentRunner>,
    /// Spawns the probe and waits for it.
    pub spawner: &'a dyn ProcessSpawner,
}

impl SpawnUsageProber<'_> {
    fn invocation(&self, agent: &str) -> Option<(&dyn AgentRunner, AgentInvocation)> {
        let runner = *self.runners.iter().find(|r| r.name() == agent)?;
        runner.usage_probe_invocation().map(|inv| (runner, inv))
    }
}

impl UsageProber for SpawnUsageProber<'_> {
    fn supports(&self, agent: &str) -> bool {
        self.invocation(agent).is_some()
    }

    fn probe(&self, agent: &str) -> Result<UsageSnapshot, String> {
        let (runner, invocation) = self
            .invocation(agent)
            .ok_or_else(|| format!("{agent} has no usage probe"))?;
        let dir = tempfile::tempdir().map_err(|err| err.to_string())?;
        let stdout_path = dir.path().join("probe.out");
        let status = self
            .spawner
            .spawn(SpawnRequest {
                program: &invocation.program,
                args: &invocation.args,
                env_set: &invocation.env_set,
                env_remove: &invocation.env_remove,
                current_dir: dir.path(),
                stdout_path: &stdout_path,
            })
            .map_err(|err| err.to_string())?;
        if !status.success() {
            return Err(format!("{} exited with {status}", invocation.program));
        }
        let raw = std::fs::read_to_string(&stdout_path).map_err(|err| err.to_string())?;
        let outcome = runner.parse_outcome(&raw).map_err(|err| err.to_string())?;
        Ok(outcome.usage.unwrap_or(UsageSnapshot {
            status: None,
            resets_at: None,
            windows: Vec::new(),
        }))
    }
}

/// Why usage admission refused a launch (GitHub issue #114). Each variant
/// carries `as_of`, the time the check ran, so the reason can say how long
/// until the window reopens.
#[derive(Debug, Clone, PartialEq)]
pub enum UsageRefusal {
    /// The agent is marked exhausted (ADR-0008's `agent_windows`, also set
    /// from a `rejected` snapshot) until `until`.
    Exhausted {
        /// `AgentRunner::name()`.
        agent: String,
        /// Unix seconds the window is expected to reopen.
        until: i64,
        /// Unix seconds the check ran.
        as_of: i64,
    },
    /// A usage window is at or above its configured ceiling.
    OverCeiling {
        /// `AgentRunner::name()`.
        agent: String,
        /// The window, e.g. `five_hour`.
        window: String,
        /// The window's utilization fraction.
        utilization: f64,
        /// The configured ceiling fraction.
        ceiling: f64,
        /// Unix seconds the window resets.
        resets_at: i64,
        /// Unix seconds the check ran.
        as_of: i64,
    },
    /// A signal-less runner used its configured token budget.
    OverTokenBudget {
        /// `AgentRunner::name()`.
        agent: String,
        /// Tokens used in the rolling window.
        used: u64,
        /// The configured budget.
        budget: u64,
        /// The rolling window, in seconds.
        window_secs: i64,
    },
}

/// Renders a positive duration in whole minutes, rounded up: `2m`,
/// `1h 12m`.
fn format_duration(secs: i64) -> String {
    let mins = (secs.max(0) + 59) / 60;
    match (mins / 60, mins % 60) {
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h {m}m"),
    }
}

/// The short name of a usage window: `5h`, `7d`, or the raw name.
fn window_label(window: &str) -> &str {
    match window {
        "five_hour" => "5h",
        "seven_day" => "7d",
        other => other,
    }
}

impl fmt::Display for UsageRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UsageRefusal::Exhausted {
                agent,
                until,
                as_of,
            } => write!(
                f,
                "not launching: {agent} usage window exhausted, reopens in {}",
                format_duration(until - as_of)
            ),
            UsageRefusal::OverCeiling {
                agent,
                window,
                utilization,
                ceiling,
                resets_at,
                as_of,
            } => write!(
                f,
                "not launching: {agent} {} usage at {:.0}% >= {:.0}% ceiling, resets in {}",
                window_label(window),
                utilization * 100.0,
                ceiling * 100.0,
                format_duration(resets_at - as_of)
            ),
            UsageRefusal::OverTokenBudget {
                agent,
                used,
                budget,
                window_secs,
            } => write!(
                f,
                "not launching: {agent} used {used} tokens in the last {}, budget {budget}",
                format_duration(*window_secs)
            ),
        }
    }
}

impl std::error::Error for UsageRefusal {}

/// The outcome of [`check_usage_admission`].
#[derive(Debug, Clone, PartialEq)]
pub enum UsageVerdict {
    /// Launch.
    Admit,
    /// Launch, but current usage couldn't be confirmed; the string says
    /// why (`usage unknown: probe failed: ...`). The reactive ADR-0008 path
    /// still catches an exhausted window at run time.
    AdmitUnknown(String),
    /// Don't launch.
    Refuse(UsageRefusal),
}

impl UsageVerdict {
    /// The refusal, if this verdict is one.
    pub fn refusal(&self) -> Option<&UsageRefusal> {
        match self {
            UsageVerdict::Refuse(refusal) => Some(refusal),
            _ => None,
        }
    }
}

/// Decides whether a lane for `agent` may launch now, given its usage
/// (GitHub issue #114; `docs/plans/gh-113-proactive-usage-signal.md`).
///
/// In order:
/// 1. A future `exhausted_until` refuses.
/// 2. If `prober` supports `agent` and there is no snapshot, or a window's
///    snapshot is older than `snapshot_max_age_secs` and hasn't reset,
///    probe once. Concurrent callers share one probe per max-age window
///    through [`RunStore::claim_usage_probe`]; a caller that loses the
///    claim judges the stored snapshot. A probe's value is stored with
///    source `probe`. A failed probe doesn't refuse; the verdict becomes
///    [`UsageVerdict::AdmitUnknown`].
/// 3. Each window counts as 0 once past its `resets_at`, and refuses at or
///    above its ceiling. A stale value still counts, since it is a lower
///    bound on current usage.
/// 4. With no usage windows at all (a signal-less runner), a configured
///    token budget refuses once the runner's runs in the rolling window
///    have used it.
///
/// # Errors
///
/// Returns a [`RunStoreError`] if the run store can't be read or written.
pub fn check_usage_admission(
    store: &RunStore,
    agent: &str,
    usage: &RunnerUsageConfig,
    now: i64,
    prober: &dyn UsageProber,
) -> Result<UsageVerdict, RunStoreError> {
    let exhausted = |store: &RunStore| -> Result<Option<UsageVerdict>, RunStoreError> {
        Ok(store
            .agent_exhausted_until(agent)?
            .filter(|until| *until > now)
            .map(|until| {
                UsageVerdict::Refuse(UsageRefusal::Exhausted {
                    agent: agent.to_string(),
                    until,
                    as_of: now,
                })
            }))
    };
    if let Some(refused) = exhausted(store)? {
        return Ok(refused);
    }

    let max_age = usage.snapshot_max_age_secs as i64;
    let mut snapshots = store.usage_snapshots(agent)?;
    let stale = snapshots.is_empty()
        || snapshots
            .iter()
            .any(|s| s.resets_at > now && now - s.observed_at > max_age);
    let mut unknown = None;
    if stale && prober.supports(agent) && store.claim_usage_probe(agent, now, max_age)? {
        match prober.probe(agent) {
            Ok(observed) => {
                crate::work::run::store_usage_snapshot(store, agent, &observed, now, "probe")?;
                if let Some(refused) = exhausted(store)? {
                    return Ok(refused);
                }
                snapshots = store.usage_snapshots(agent)?;
            }
            Err(err) => unknown = Some(format!("usage unknown: probe failed: {err}")),
        }
    }

    for snapshot in &snapshots {
        let effective = if now >= snapshot.resets_at {
            0.0
        } else {
            snapshot.utilization
        };
        if let Some(ceiling) = usage.ceiling_for(&snapshot.window)
            && effective >= ceiling.get()
        {
            return Ok(UsageVerdict::Refuse(UsageRefusal::OverCeiling {
                agent: agent.to_string(),
                window: snapshot.window.clone(),
                utilization: effective,
                ceiling: ceiling.get(),
                resets_at: snapshot.resets_at,
                as_of: now,
            }));
        }
    }

    if snapshots.is_empty()
        && let Some(budget) = usage.token_budget
    {
        let used = store.agent_tokens_since(agent, now - budget.window_secs)?;
        if used >= budget.tokens {
            return Ok(UsageVerdict::Refuse(UsageRefusal::OverTokenBudget {
                agent: agent.to_string(),
                used,
                budget: budget.tokens,
                window_secs: budget.window_secs,
            }));
        }
    }

    Ok(match unknown {
        Some(reason) => UsageVerdict::AdmitUnknown(reason),
        None => UsageVerdict::Admit,
    })
}

/// How a caller acts on a [`UsageVerdict`] (GitHub issue #114).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageGateMode {
    /// Print a refusal as a warning and launch anyway. A manual `tm work
    /// run` uses this: a human chose to launch.
    Warn,
    /// Refuse. Unattended dispatch (GitHub issue #115) uses this.
    Enforce,
}

/// Applies `verdict` in `mode`: writes any warning to `out`, and returns
/// the refusal only in [`UsageGateMode::Enforce`]. Unknown usage is
/// reported in both modes and never blocks.
pub fn apply_usage_gate(
    verdict: UsageVerdict,
    mode: UsageGateMode,
    out: &mut dyn Write,
) -> Result<(), UsageRefusal> {
    match (verdict, mode) {
        (UsageVerdict::Admit, _) => Ok(()),
        (UsageVerdict::AdmitUnknown(reason), _) => {
            let _ = writeln!(out, "warning: {reason}");
            Ok(())
        }
        (UsageVerdict::Refuse(refusal), UsageGateMode::Warn) => {
            let _ = writeln!(out, "warning: {refusal} (launching anyway)");
            Ok(())
        }
        (UsageVerdict::Refuse(refusal), UsageGateMode::Enforce) => Err(refusal),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runs::footprint::GIB;
    use crate::runs::{FinishRun, RunStatus, StartRun};
    use tempfile::tempdir;

    fn store(dir: &std::path::Path) -> RunStore {
        RunStore::open(&dir.join("runs.db")).unwrap()
    }

    fn budget(gb: u64) -> MemoryConfig {
        MemoryConfig {
            budget_bytes: Some(gb * GIB),
            default_lane_estimate_bytes: 2 * GIB,
        }
    }

    fn normal() -> MemoryPressure {
        MemoryPressure::Normal
    }

    fn start_lane(store: &RunStore, agent: &str, repo: &str) -> i64 {
        let id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                scope: String::new(),
                lane: "lane".to_string(),
                worktree: "/tmp/wt".to_string(),
                branch: None,
                pid: None,
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();
        store.update_agent_repo(id, agent, repo).unwrap();
        id
    }

    fn finished_lane(store: &RunStore, agent: &str, repo: &str, peak: u64) {
        let id = start_lane(store, agent, repo);
        store.record_footprint(id, peak).unwrap();
        store
            .finish_run(
                id,
                &FinishRun {
                    status: RunStatus::Done,
                    ..FinishRun::default()
                },
            )
            .unwrap();
    }

    #[test]
    fn admission_is_off_without_a_budget_even_under_pressure() {
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        for _ in 0..20 {
            start_lane(&store, "opencode", "lemma");
        }

        let verdict = check_lane_admission(
            &store,
            &MemoryConfig::default(),
            "opencode",
            "lemma",
            &|| panic!("pressure must not be read with admission off"),
        )
        .unwrap();

        assert_eq!(verdict, None);
    }

    #[test]
    fn pressure_refuses_a_launch_whatever_the_budget_says() {
        let dir = tempdir().unwrap();
        let store = store(dir.path());

        let verdict = check_lane_admission(&store, &budget(1000), "claude", "tskmstr", &|| {
            MemoryPressure::Warn
        })
        .unwrap();

        assert_eq!(
            verdict,
            Some(AdmissionRefusal::Pressure(MemoryPressure::Warn))
        );
    }

    #[test]
    fn a_launch_that_fits_the_budget_is_admitted() {
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        start_lane(&store, "claude", "tskmstr");

        let verdict =
            check_lane_admission(&store, &budget(4), "claude", "tskmstr", &normal).unwrap();

        assert_eq!(verdict, None, "2 GB running + 2 GB new fits 4 GB");
    }

    #[test]
    fn an_over_budget_launch_is_refused_with_its_numbers() {
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        finished_lane(&store, "opencode", "lemma", 5 * GIB / 2);
        for _ in 0..7 {
            start_lane(&store, "opencode", "lemma");
        }

        // 7 x 2.5 GB running + 2.5 GB new = 20 GB: fits exactly.
        let verdict =
            check_lane_admission(&store, &budget(20), "opencode", "lemma", &normal).unwrap();
        assert_eq!(verdict, None);

        start_lane(&store, "opencode", "lemma");
        let verdict = check_lane_admission(&store, &budget(20), "opencode", "lemma", &normal)
            .unwrap()
            .expect("8 x 2.5 GB running + 2.5 GB new exceeds 20 GB");

        let AdmissionRefusal::OverBudget(over) = &verdict else {
            panic!("expected OverBudget, got {verdict:?}");
        };
        assert_eq!(over.estimate_bytes, 5 * GIB / 2);
        assert!(over.from_history);
        assert_eq!(over.running_count, 8);
        assert_eq!(over.running_bytes, 8 * 5 * GIB / 2);
        assert_eq!(
            verdict.to_string(),
            "not launching: memory budget exceeded: lemma/opencode lanes peak ~2.5 GB; \
             8 running (~20.0 GB); budget 20.0 GB"
        );
    }

    #[test]
    fn a_lane_with_no_history_is_estimated_at_the_configured_default() {
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        start_lane(&store, "claude", "other");

        let verdict = check_lane_admission(&store, &budget(3), "claude", "tskmstr", &normal)
            .unwrap()
            .unwrap();

        assert_eq!(
            verdict.to_string(),
            "not launching: memory budget exceeded: tskmstr/claude lanes have no recorded \
             peak, assuming ~2.0 GB; 1 running (~2.0 GB); budget 3.0 GB"
        );
    }

    #[test]
    fn a_running_lane_counts_at_its_own_peak_when_that_exceeds_its_estimate() {
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        let id = start_lane(&store, "claude", "tskmstr");
        store.record_footprint(id, 6 * GIB).unwrap();

        let verdict = check_lane_admission(&store, &budget(7), "claude", "tskmstr", &normal)
            .unwrap()
            .unwrap();

        let AdmissionRefusal::OverBudget(over) = verdict else {
            panic!("expected OverBudget");
        };
        assert_eq!(over.running_bytes, 6 * GIB);
    }

    #[test]
    fn a_finished_lane_frees_its_share() {
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        finished_lane(&store, "claude", "tskmstr", GIB);
        finished_lane(&store, "claude", "tskmstr", GIB);

        let verdict =
            check_lane_admission(&store, &budget(1), "claude", "tskmstr", &normal).unwrap();

        assert_eq!(verdict, None);
    }

    #[test]
    fn running_estimate_sums_each_lane_at_its_estimate_or_peak() {
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        finished_lane(&store, "claude", "tskmstr", GIB);
        start_lane(&store, "claude", "tskmstr");
        let big = start_lane(&store, "claude", "tskmstr");
        store.record_footprint(big, 6 * GIB).unwrap();
        start_lane(&store, "opencode", "lemma");

        let running = running_lane_estimate(&store, &budget(100)).unwrap();

        assert_eq!(
            running,
            RunningEstimate {
                count: 3,
                bytes: GIB + 6 * GIB + 2 * GIB,
            }
        );
    }
}

#[cfg(test)]
mod usage_tests {
    use std::cell::{Cell, RefCell};

    use super::*;
    use crate::agent::{UsageSnapshot, UsageWindow};
    use crate::config::{AgentKind, RunnerUsageConfig, TokenBudget};
    use crate::runs::{FinishRun, RunStatus, StartRun, UsageSnapshotRow};
    use tempfile::tempdir;

    const NOW: i64 = 1_000_000;
    const HOUR: i64 = 3600;

    fn store(dir: &std::path::Path) -> RunStore {
        RunStore::open(&dir.join("runs.db")).unwrap()
    }

    fn claude() -> RunnerUsageConfig {
        RunnerUsageConfig::defaults_for(AgentKind::Claude)
    }

    fn opencode() -> RunnerUsageConfig {
        RunnerUsageConfig::defaults_for(AgentKind::Opencode)
    }

    fn seed(store: &RunStore, window: &str, utilization: f64, resets_at: i64, observed_at: i64) {
        store
            .upsert_usage_snapshot(&UsageSnapshotRow {
                agent: "claude".to_string(),
                window: window.to_string(),
                utilization,
                resets_at,
                observed_at,
                source: "stream".to_string(),
                status: Some("allowed".to_string()),
            })
            .unwrap();
    }

    /// A [`UsageProber`] that counts its calls and answers with `result`.
    struct FakeProber {
        supported: bool,
        result: RefCell<Result<UsageSnapshot, String>>,
        calls: Cell<usize>,
    }

    impl FakeProber {
        fn answering(result: Result<UsageSnapshot, String>) -> Self {
            FakeProber {
                supported: true,
                result: RefCell::new(result),
                calls: Cell::new(0),
            }
        }

        fn unsupported() -> Self {
            FakeProber {
                supported: false,
                ..FakeProber::answering(Err("must not probe".to_string()))
            }
        }

        fn never() -> Self {
            FakeProber::answering(Err("must not probe".to_string()))
        }
    }

    impl UsageProber for FakeProber {
        fn supports(&self, _agent: &str) -> bool {
            self.supported
        }

        fn probe(&self, _agent: &str) -> Result<UsageSnapshot, String> {
            self.calls.set(self.calls.get() + 1);
            self.result.borrow().clone()
        }
    }

    fn observed(five_hour: f64, status: &str) -> UsageSnapshot {
        UsageSnapshot {
            status: Some(status.to_string()),
            resets_at: Some(NOW + HOUR),
            windows: vec![
                UsageWindow {
                    window: "five_hour".to_string(),
                    utilization: five_hour,
                    resets_at: NOW + HOUR,
                },
                UsageWindow {
                    window: "seven_day".to_string(),
                    utilization: 0.1,
                    resets_at: NOW + 100 * HOUR,
                },
            ],
        }
    }

    fn check(store: &RunStore, cfg: &RunnerUsageConfig, prober: &FakeProber) -> UsageVerdict {
        check_usage_admission(store, "claude", cfg, NOW, prober).unwrap()
    }

    #[test]
    fn fresh_snapshots_below_both_ceilings_admit_without_probing() {
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        seed(&store, "five_hour", 0.79, NOW + HOUR, NOW - 10);
        seed(&store, "seven_day", 0.89, NOW + 100 * HOUR, NOW - 10);
        let prober = FakeProber::never();

        assert_eq!(check(&store, &claude(), &prober), UsageVerdict::Admit);
        assert_eq!(prober.calls.get(), 0);
    }

    #[test]
    fn a_window_at_its_ceiling_refuses_with_a_readable_reason() {
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        seed(&store, "five_hour", 0.83, NOW + HOUR + 12 * 60, NOW - 10);
        seed(&store, "seven_day", 0.2, NOW + 100 * HOUR, NOW - 10);

        let verdict = check(&store, &claude(), &FakeProber::never());

        let UsageVerdict::Refuse(refusal) = verdict else {
            panic!("expected a refusal, got {verdict:?}");
        };
        assert_eq!(
            refusal.to_string(),
            "not launching: claude 5h usage at 83% >= 80% ceiling, resets in 1h 12m"
        );
    }

    #[test]
    fn the_seven_day_ceiling_refuses_at_exactly_its_value() {
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        seed(&store, "seven_day", 0.90, NOW + 100 * HOUR, NOW - 10);

        let verdict = check(&store, &claude(), &FakeProber::never());

        assert!(
            matches!(
                verdict,
                UsageVerdict::Refuse(UsageRefusal::OverCeiling { ref window, .. })
                    if window == "seven_day"
            ),
            "{verdict:?}"
        );
    }

    #[test]
    fn a_snapshot_past_its_reset_counts_as_zero_and_is_not_probed() {
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        seed(&store, "five_hour", 0.99, NOW, NOW - 6 * HOUR);
        let prober = FakeProber::never();

        assert_eq!(check(&store, &claude(), &prober), UsageVerdict::Admit);
        assert_eq!(prober.calls.get(), 0);
    }

    #[test]
    fn a_future_exhausted_until_refuses_and_a_past_one_does_not() {
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        seed(&store, "five_hour", 0.1, NOW + HOUR, NOW);
        store.set_agent_exhausted_until("claude", NOW + 90).unwrap();

        let verdict = check(&store, &claude(), &FakeProber::never());
        assert_eq!(
            verdict,
            UsageVerdict::Refuse(UsageRefusal::Exhausted {
                agent: "claude".to_string(),
                until: NOW + 90,
                as_of: NOW,
            })
        );
        assert_eq!(
            verdict.refusal().unwrap().to_string(),
            "not launching: claude usage window exhausted, reopens in 2m"
        );

        store.set_agent_exhausted_until("claude", NOW).unwrap();
        assert_eq!(
            check(&store, &claude(), &FakeProber::never()),
            UsageVerdict::Admit
        );
    }

    #[test]
    fn a_stale_snapshot_triggers_exactly_one_probe_whose_value_decides() {
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        seed(&store, "five_hour", 0.1, NOW + HOUR, NOW - 601);
        let prober = FakeProber::answering(Ok(observed(0.85, "allowed_warning")));

        let verdict = check(&store, &claude(), &prober);

        assert_eq!(prober.calls.get(), 1);
        assert!(
            matches!(
                verdict,
                UsageVerdict::Refuse(UsageRefusal::OverCeiling { .. })
            ),
            "{verdict:?}"
        );
        let rows = store.usage_snapshots("claude").unwrap();
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .all(|r| r.source == "probe" && r.observed_at == NOW)
        );
    }

    #[test]
    fn no_snapshot_at_all_is_probed_too() {
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        let prober = FakeProber::answering(Ok(observed(0.2, "allowed")));

        assert_eq!(check(&store, &claude(), &prober), UsageVerdict::Admit);
        assert_eq!(prober.calls.get(), 1);
    }

    #[test]
    fn a_rejected_probe_marks_the_agent_exhausted_and_refuses() {
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        let prober = FakeProber::answering(Ok(observed(0.5, "rejected")));

        let verdict = check(&store, &claude(), &prober);

        assert_eq!(
            verdict,
            UsageVerdict::Refuse(UsageRefusal::Exhausted {
                agent: "claude".to_string(),
                until: NOW + HOUR,
                as_of: NOW,
            })
        );
        assert_eq!(
            store.agent_exhausted_until("claude").unwrap(),
            Some(NOW + HOUR)
        );
    }

    #[test]
    fn a_failed_probe_degrades_to_unknown_and_does_not_block() {
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        seed(&store, "five_hour", 0.1, NOW + HOUR, NOW - 601);
        let prober = FakeProber::answering(Err("claude exited 1".to_string()));

        let verdict = check(&store, &claude(), &prober);

        assert_eq!(
            verdict,
            UsageVerdict::AdmitUnknown("usage unknown: probe failed: claude exited 1".to_string())
        );
        assert!(verdict.refusal().is_none());
    }

    #[test]
    fn a_failed_probe_still_refuses_on_a_stale_value_over_the_ceiling() {
        // A stale value is a lower bound: usage only rises until the reset.
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        seed(&store, "five_hour", 0.95, NOW + HOUR, NOW - 601);
        let prober = FakeProber::answering(Err("boom".to_string()));

        let verdict = check(&store, &claude(), &prober);

        assert!(verdict.refusal().is_some(), "{verdict:?}");
    }

    #[test]
    fn concurrent_callers_share_one_probe_per_max_age_window() {
        let dir = tempdir().unwrap();
        let first = store(dir.path());
        let second = store(dir.path());
        seed(&first, "five_hour", 0.1, NOW + HOUR, NOW - 601);
        let failing = FakeProber::answering(Err("boom".to_string()));
        let other = FakeProber::answering(Err("boom".to_string()));

        check_usage_admission(&first, "claude", &claude(), NOW, &failing).unwrap();
        let verdict = check_usage_admission(&second, "claude", &claude(), NOW + 1, &other).unwrap();

        assert_eq!(failing.calls.get(), 1);
        assert_eq!(other.calls.get(), 0, "the first caller's claim covers it");
        assert_eq!(verdict, UsageVerdict::Admit);

        check_usage_admission(&second, "claude", &claude(), NOW + 600, &other).unwrap();
        assert_eq!(
            other.calls.get(),
            1,
            "a new max-age window allows a new probe"
        );
    }

    fn finished_run(store: &RunStore, agent: &str, input_tokens: u64) {
        let id = store
            .start_run(&StartRun {
                ticket: "PROJ-1".to_string(),
                scope: String::new(),
                lane: "lane".to_string(),
                worktree: "/tmp/wt".to_string(),
                branch: None,
                pid: None,
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();
        store.update_agent_repo(id, agent, "repo").unwrap();
        store
            .finish_run(
                id,
                &FinishRun {
                    status: RunStatus::Done,
                    model_usage: Some(format!(r#"{{"m":{{"inputTokens":{input_tokens}}}}}"#)),
                    ..FinishRun::default()
                },
            )
            .unwrap();
    }

    fn with_budget(mut cfg: RunnerUsageConfig, tokens: u64) -> RunnerUsageConfig {
        cfg.token_budget = Some(TokenBudget {
            tokens,
            window_secs: 5 * HOUR,
        });
        cfg
    }

    #[test]
    fn a_signal_less_runner_has_no_gate_without_a_budget() {
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        finished_run(&store, "opencode", 1_000_000);

        let verdict = check_usage_admission(
            &store,
            "opencode",
            &opencode(),
            NOW,
            &FakeProber::unsupported(),
        )
        .unwrap();

        assert_eq!(verdict, UsageVerdict::Admit);
    }

    #[test]
    fn a_configured_budget_gates_a_signal_less_runner() {
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        finished_run(&store, "opencode", 600);
        finished_run(&store, "opencode", 400);
        // Runs are stamped with the real clock, so judge them at the real now.
        let now = crate::work::review_watch::Clock::now_unix_secs(
            &crate::work::review_watch::SystemClock,
        );

        let under = check_usage_admission(
            &store,
            "opencode",
            &with_budget(opencode(), 1001),
            now,
            &FakeProber::unsupported(),
        )
        .unwrap();
        assert_eq!(under, UsageVerdict::Admit);

        let at = check_usage_admission(
            &store,
            "opencode",
            &with_budget(opencode(), 1000),
            now,
            &FakeProber::unsupported(),
        )
        .unwrap();
        assert_eq!(
            at.refusal().unwrap().to_string(),
            "not launching: opencode used 1000 tokens in the last 5h, budget 1000"
        );
    }

    #[test]
    fn the_budget_is_ignored_while_usage_snapshots_exist() {
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        seed(&store, "five_hour", 0.1, NOW + HOUR, NOW - 10);
        finished_run(&store, "claude", 5_000);

        let verdict = check(&store, &with_budget(claude(), 1), &FakeProber::never());

        assert_eq!(verdict, UsageVerdict::Admit);
    }

    #[test]
    fn a_probe_reporting_no_windows_falls_back_to_the_budget() {
        // claude under an API key: the probe succeeds but carries no
        // rate_limits, so the runner is signal-less.
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        finished_run(&store, "claude", 5_000);
        let now = crate::work::review_watch::Clock::now_unix_secs(
            &crate::work::review_watch::SystemClock,
        );
        let prober = FakeProber::answering(Ok(UsageSnapshot {
            status: None,
            resets_at: None,
            windows: Vec::new(),
        }));

        let verdict =
            check_usage_admission(&store, "claude", &with_budget(claude(), 10), now, &prober)
                .unwrap();

        assert!(
            matches!(
                verdict,
                UsageVerdict::Refuse(UsageRefusal::OverTokenBudget { .. })
            ),
            "{verdict:?}"
        );
    }

    // --- gate modes ---

    fn refused() -> UsageVerdict {
        UsageVerdict::Refuse(UsageRefusal::Exhausted {
            agent: "claude".to_string(),
            until: NOW,
            as_of: NOW,
        })
    }

    #[test]
    fn warn_mode_reports_a_refusal_but_launches() {
        let mut out = Vec::new();

        let result = apply_usage_gate(refused(), UsageGateMode::Warn, &mut out);

        assert!(result.is_ok());
        let out = String::from_utf8(out).unwrap();
        assert!(out.starts_with("warning: "), "{out}");
        assert!(out.contains("launching anyway"), "{out}");
    }

    #[test]
    fn enforce_mode_returns_the_refusal() {
        let mut out = Vec::new();

        let result = apply_usage_gate(refused(), UsageGateMode::Enforce, &mut out);

        assert!(matches!(result, Err(UsageRefusal::Exhausted { .. })));
    }

    #[test]
    fn both_modes_report_unknown_usage_and_launch() {
        for mode in [UsageGateMode::Warn, UsageGateMode::Enforce] {
            let mut out = Vec::new();
            let verdict = UsageVerdict::AdmitUnknown("usage unknown: probe failed: x".to_string());

            assert!(apply_usage_gate(verdict, mode, &mut out).is_ok());
            assert_eq!(
                String::from_utf8(out).unwrap(),
                "warning: usage unknown: probe failed: x\n"
            );
        }
    }
}
