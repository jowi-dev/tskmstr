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

use std::collections::HashMap;
use std::fmt;

use crate::config::MemoryConfig;
use crate::runs::footprint::{MemoryPressure, format_bytes};
use crate::runs::{RunStore, RunStoreError};

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

    let mut estimates: HashMap<(String, String), Option<u64>> = HashMap::new();
    let mut estimate_for = |agent: &str, repo: &str| -> Result<Option<u64>, RunStoreError> {
        let key = (agent.to_string(), repo.to_string());
        if let Some(cached) = estimates.get(&key) {
            return Ok(*cached);
        }
        let estimate = store.lane_peak_estimate(agent, repo)?;
        estimates.insert(key, estimate);
        Ok(estimate)
    };

    let running = store.running_lanes()?;
    let mut running_bytes = 0u64;
    for lane in &running {
        let key_estimate = match (&lane.agent, &lane.repo) {
            (Some(agent), Some(repo)) => estimate_for(agent, repo)?,
            _ => None,
        };
        let estimate = key_estimate.unwrap_or(memory.default_lane_estimate_bytes);
        running_bytes += estimate.max(lane.mem_peak_bytes.unwrap_or(0));
    }

    let history = estimate_for(agent, repo)?;
    let estimate_bytes = history.unwrap_or(memory.default_lane_estimate_bytes);
    if running_bytes + estimate_bytes <= budget_bytes {
        return Ok(None);
    }

    Ok(Some(AdmissionRefusal::OverBudget(OverBudget {
        agent: agent.to_string(),
        repo: repo.to_string(),
        estimate_bytes,
        from_history: history.is_some(),
        running_count: running.len(),
        running_bytes,
        budget_bytes,
    })))
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
}
