//! The overview's system header (GitHub issue #86, ADR-0009 decision 7):
//! one line answering "can I launch another lane".
//!
//! It shows CPU and memory summed across the running lanes, against the
//! host's cores and memory; the kernel's memory-pressure level; the
//! `[work]` memory budget against the running lanes' estimated cost (#66);
//! and `build_slots` in use out of the total (#65). Any figure that can't
//! be read on this platform renders as `?`. It is never an error.

use std::time::{Duration, Instant};

use super::RunningLane;
use super::cpu::CpuSampler;
use super::footprint::{MemoryPressure, format_bytes};

/// Everything the header line shows.
#[derive(Debug, Clone, PartialEq)]
pub struct SystemHeader {
    /// How many lanes are running.
    pub lanes: usize,
    /// The running lanes' summed CPU use, in percent of one core, or `None`
    /// when unknown; see [`CpuSampler::sample`].
    pub lane_cpu_percent: Option<f64>,
    /// The running lanes' summed latest footprint, or `None` when no
    /// running lane has a sample yet.
    pub lane_mem_bytes: Option<u64>,
    /// The host's CPU count.
    pub host_cores: usize,
    /// The host's physical memory, or `None` when unknown.
    pub host_mem_bytes: Option<u64>,
    /// The kernel's memory-pressure level.
    pub pressure: MemoryPressure,
    /// The memory budget's use, or `None` when no budget is configured.
    pub budget: Option<BudgetUse>,
    /// Build slots in use, or `None` when `build_slots` is off.
    pub build_slots: Option<SlotUse>,
}

/// The running lanes' estimated cost against the configured memory budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetUse {
    /// See [`crate::work::admission::RunningEstimate::bytes`].
    pub committed_bytes: u64,
    /// The configured budget.
    pub budget_bytes: u64,
}

/// Build slots held right now out of the configured total.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotUse {
    /// Slots whose lock is held.
    pub in_use: u32,
    /// The configured `build_slots`.
    pub total: u32,
}

/// The running lanes' summed CPU percent (sampled through `sampler` at
/// `now`) and summed latest footprint. Memory is `None` when lanes run but
/// none has a footprint sample; lanes without one are left out of the sum.
pub fn lane_totals(
    lanes: &[RunningLane],
    sampler: &mut CpuSampler,
    now: Instant,
    cpu_time: &dyn Fn(u32) -> Option<Duration>,
) -> (Option<f64>, Option<u64>) {
    let pids: Vec<u32> = lanes.iter().filter_map(|lane| lane.pid).collect();
    let cpu = sampler.sample(&pids, now, cpu_time);
    let samples: Vec<u64> = lanes
        .iter()
        .filter_map(|lane| lane.mem_current_bytes)
        .collect();
    let mem = (lanes.is_empty() || !samples.is_empty()).then(|| samples.iter().sum());
    (cpu, mem)
}

impl SystemHeader {
    /// The header line, e.g. `3 lanes · cpu 240% of 10 cores · mem 6.2 GB
    /// of 32.0 GB · pressure normal · budget 9.0 GB of 24.0 GB · build
    /// slots 1/2`.
    pub fn summary(&self) -> String {
        let bytes = |b: Option<u64>| b.map_or_else(|| "?".to_string(), format_bytes);
        let lanes = match self.lanes {
            1 => "1 lane".to_string(),
            n => format!("{n} lanes"),
        };
        let cpu = self
            .lane_cpu_percent
            .map_or_else(|| "?".to_string(), |pct| format!("{pct:.0}%"));
        let pressure = match self.pressure {
            MemoryPressure::Unknown => "?",
            level => level.as_str(),
        };
        let budget = self.budget.map_or_else(
            || "off".to_string(),
            |b| {
                format!(
                    "{} of {}",
                    format_bytes(b.committed_bytes),
                    format_bytes(b.budget_bytes)
                )
            },
        );
        let slots = self.build_slots.map_or_else(
            || "off".to_string(),
            |s| format!("{}/{}", s.in_use, s.total),
        );
        format!(
            "{lanes} · cpu {cpu} of {} cores · mem {} of {} · pressure {pressure} \
             · budget {budget} · build slots {slots}",
            self.host_cores,
            bytes(self.lane_mem_bytes),
            bytes(self.host_mem_bytes),
        )
    }

    /// Whether a new lane launch would be refused right now: the kernel
    /// reports warn or critical pressure, or the running lanes already fill
    /// the budget. The header is drawn as a warning when this holds.
    pub fn launch_blocked(&self) -> bool {
        self.pressure.blocks_launch()
            || self
                .budget
                .is_some_and(|b| b.committed_bytes >= b.budget_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runs::footprint::GIB;

    fn lane(id: i64, pid: Option<u32>, mem: Option<u64>) -> RunningLane {
        RunningLane {
            id,
            agent: None,
            repo: None,
            mem_peak_bytes: mem,
            mem_current_bytes: mem,
            pid,
        }
    }

    fn header() -> SystemHeader {
        SystemHeader {
            lanes: 3,
            lane_cpu_percent: Some(240.4),
            lane_mem_bytes: Some(6 * GIB + GIB / 5),
            host_cores: 10,
            host_mem_bytes: Some(32 * GIB),
            pressure: MemoryPressure::Normal,
            budget: Some(BudgetUse {
                committed_bytes: 9 * GIB,
                budget_bytes: 24 * GIB,
            }),
            build_slots: Some(SlotUse {
                in_use: 1,
                total: 2,
            }),
        }
    }

    #[test]
    fn summary_shows_every_figure() {
        assert_eq!(
            header().summary(),
            "3 lanes · cpu 240% of 10 cores · mem 6.2 GB of 32.0 GB · pressure normal \
             · budget 9.0 GB of 24.0 GB · build slots 1/2"
        );
    }

    #[test]
    fn summary_marks_unknowns_and_off_settings() {
        let header = SystemHeader {
            lanes: 1,
            lane_cpu_percent: None,
            lane_mem_bytes: None,
            host_mem_bytes: None,
            pressure: MemoryPressure::Unknown,
            budget: None,
            build_slots: None,
            ..header()
        };
        assert_eq!(
            header.summary(),
            "1 lane · cpu ? of 10 cores · mem ? of ? · pressure ? · budget off \
             · build slots off"
        );
    }

    #[test]
    fn launch_is_blocked_by_pressure_or_a_full_budget() {
        assert!(!header().launch_blocked());
        assert!(
            SystemHeader {
                pressure: MemoryPressure::Warn,
                ..header()
            }
            .launch_blocked()
        );
        assert!(
            SystemHeader {
                budget: Some(BudgetUse {
                    committed_bytes: 24 * GIB,
                    budget_bytes: 24 * GIB,
                }),
                ..header()
            }
            .launch_blocked()
        );
        assert!(
            !SystemHeader {
                budget: None,
                pressure: MemoryPressure::Unknown,
                ..header()
            }
            .launch_blocked()
        );
    }

    #[test]
    fn lane_totals_sum_memory_and_sample_cpu_of_lanes_with_pids() {
        let lanes = [
            lane(1, Some(10), Some(GIB)),
            lane(2, None, Some(2 * GIB)),
            lane(3, Some(30), None),
        ];
        let mut sampler = CpuSampler::new();
        let start = Instant::now();
        let seen = std::cell::RefCell::new(Vec::new());
        let cpu = |pid| {
            seen.borrow_mut().push(pid);
            Some(Duration::ZERO)
        };

        let (cpu_pct, mem) = lane_totals(&lanes, &mut sampler, start, &cpu);
        assert_eq!(cpu_pct, None, "first sample is unknown");
        assert_eq!(mem, Some(3 * GIB));
        assert_eq!(*seen.borrow(), vec![10, 30]);

        let (cpu_pct, _) = lane_totals(&lanes, &mut sampler, start + Duration::from_secs(1), &cpu);
        assert_eq!(cpu_pct, Some(0.0));
    }

    #[test]
    fn lane_memory_is_unknown_until_a_lane_has_a_sample() {
        let mut sampler = CpuSampler::new();
        let (_, mem) = lane_totals(
            &[lane(1, None, None)],
            &mut sampler,
            Instant::now(),
            &|_| None,
        );
        assert_eq!(mem, None);

        let (cpu, mem) = lane_totals(&[], &mut sampler, Instant::now(), &|_| None);
        assert_eq!((cpu, mem), (Some(0.0), Some(0)));
    }
}
