//! CPU measurement for running lanes and host totals for the overview's
//! system header (GitHub issue #86, ADR-0009 decision 7).
//!
//! A lane's CPU use is sampled per process tree, the same way
//! [`super::footprint`] measures memory: [`tree_cpu_time`] sums the
//! cumulative CPU time (user plus system) of a root pid and every process
//! descended from it, and [`CpuSampler`] turns two of those readings into a
//! percentage of one core over the interval between them.
//!
//! The probes read macOS's `proc_pid_rusage` and Linux's `/proc`. Elsewhere
//! they report "unknown" ([`tree_cpu_time`] and [`host_memory_bytes`]
//! return `None`), which callers treat as "no data" rather than as an error.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Returns the summed CPU time (user plus system) that `root` and every
/// process descended from it have used so far, or `None` when `root` is not
/// running (or on a platform with no probe).
///
/// Like [`super::footprint::tree_footprint`], descendants that exit between
/// listing and measuring are skipped, and their CPU time drops out of the
/// sum. [`CpuSampler`] clamps the resulting dip to zero.
pub fn tree_cpu_time(root: u32) -> Option<Duration> {
    imp::tree_cpu_time(root)
}

/// The host's physical memory, in bytes, or `None` where it can't be read.
pub fn host_memory_bytes() -> Option<u64> {
    imp::host_memory_bytes()
}

/// The number of CPUs this process may run on, at least one.
pub fn host_cores() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

/// Every live process's direct children, from one `/proc` scan, shared
/// with [`super::footprint`]'s tree walk.
#[cfg(target_os = "linux")]
pub(crate) use imp::children_by_parent;

/// Turns successive [`tree_cpu_time`] readings into CPU percentages. Each
/// pid's previous reading is kept between calls to [`CpuSampler::sample`].
#[derive(Debug, Default)]
pub struct CpuSampler {
    last: HashMap<u32, (Instant, Duration)>,
}

impl CpuSampler {
    /// A sampler with no previous readings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads `cpu_time` for each of `pids` at `now` and returns their summed
    /// CPU use since the previous call, in percent of one core (`250.0` is
    /// two and a half cores busy).
    ///
    /// Returns `Some(0.0)` for no pids, and `None` ("unknown") when there
    /// are pids but none has both a previous and a current reading: on the
    /// first call, or where `cpu_time` can't read anything. Pids absent from
    /// `pids` are forgotten.
    pub fn sample(
        &mut self,
        pids: &[u32],
        now: Instant,
        cpu_time: &dyn Fn(u32) -> Option<Duration>,
    ) -> Option<f64> {
        self.last.retain(|pid, _| pids.contains(pid));
        if pids.is_empty() {
            return Some(0.0);
        }
        let mut percent = None;
        for &pid in pids {
            let Some(used) = cpu_time(pid) else {
                self.last.remove(&pid);
                continue;
            };
            if let Some((then, used_then)) = self.last.insert(pid, (now, used)) {
                let wall = now.saturating_duration_since(then).as_secs_f64();
                if wall > 0.0 {
                    let busy = used.saturating_sub(used_then).as_secs_f64();
                    *percent.get_or_insert(0.0) += busy / wall * 100.0;
                }
            }
        }
        percent
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::time::Duration;

    use crate::runs::footprint::child_pids;

    pub fn tree_cpu_time(root: u32) -> Option<Duration> {
        let mut total = process_cpu_ticks(root)?;
        let mut pending = child_pids(root);
        while let Some(pid) = pending.pop() {
            if let Some(ticks) = process_cpu_ticks(pid) {
                total += ticks;
                pending.extend(child_pids(pid));
            }
        }
        Some(Duration::from_nanos(ticks_to_nanos(total)))
    }

    /// User plus system time, in mach absolute-time units.
    fn process_cpu_ticks(pid: u32) -> Option<u64> {
        let mut info = std::mem::MaybeUninit::<libc::rusage_info_v2>::zeroed();
        // SAFETY: `info` is a properly sized, writable `rusage_info_v2`,
        // which is what the RUSAGE_INFO_V2 flavor fills in.
        let rc = unsafe {
            libc::proc_pid_rusage(
                pid as libc::c_int,
                libc::RUSAGE_INFO_V2,
                info.as_mut_ptr().cast::<libc::rusage_info_t>(),
            )
        };
        if rc != 0 {
            return None;
        }
        // SAFETY: a zero return means the kernel filled the struct.
        let info = unsafe { info.assume_init() };
        Some(info.ri_user_time + info.ri_system_time)
    }

    /// Converts mach absolute-time units to nanoseconds. They are equal on
    /// Intel and differ on Apple silicon.
    #[allow(deprecated)]
    fn ticks_to_nanos(ticks: u64) -> u64 {
        let mut timebase = libc::mach_timebase_info { numer: 0, denom: 0 };
        // SAFETY: `timebase` is a writable `mach_timebase_info`.
        let rc = unsafe { libc::mach_timebase_info(&mut timebase) };
        if rc != 0 || timebase.denom == 0 {
            return ticks;
        }
        (u128::from(ticks) * u128::from(timebase.numer) / u128::from(timebase.denom)) as u64
    }

    pub fn host_memory_bytes() -> Option<u64> {
        let mut bytes: u64 = 0;
        let mut len = std::mem::size_of::<u64>();
        // SAFETY: the name is a NUL-terminated C string, and `bytes`/`len`
        // describe a writable `u64`, the size `hw.memsize` returns.
        let rc = unsafe {
            libc::sysctlbyname(
                c"hw.memsize".as_ptr(),
                (&mut bytes as *mut u64).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        (rc == 0 && bytes > 0).then_some(bytes)
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::collections::HashMap;
    use std::time::Duration;

    /// Walks the tree from one `/proc` scan of every process's parent, so
    /// it doesn't need `CONFIG_PROC_CHILDREN`.
    pub fn tree_cpu_time(root: u32) -> Option<Duration> {
        let (_, root_ticks) = read_stat(root)?;
        let mut children: HashMap<u32, Vec<(u32, u64)>> = HashMap::new();
        for entry in std::fs::read_dir("/proc").ok()?.flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse().ok()) else {
                continue;
            };
            if let Some((ppid, ticks)) = read_stat(pid) {
                children.entry(ppid).or_default().push((pid, ticks));
            }
        }

        let mut total = root_ticks;
        let mut pending = vec![root];
        while let Some(pid) = pending.pop() {
            for &(child, ticks) in children.get(&pid).into_iter().flatten() {
                total += ticks;
                pending.push(child);
            }
        }
        Some(ticks_to_duration(total))
    }

    /// Maps each parent pid to its direct children, from one `/proc` scan of
    /// every process's parent. `None` when `/proc` can't be listed.
    pub fn children_by_parent() -> Option<HashMap<u32, Vec<u32>>> {
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for entry in std::fs::read_dir("/proc").ok()?.flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse().ok()) else {
                continue;
            };
            if let Some((ppid, _)) = read_stat(pid) {
                children.entry(ppid).or_default().push(pid);
            }
        }
        Some(children)
    }

    /// A process's parent pid and its user plus system time in clock ticks,
    /// from `/proc/<pid>/stat`.
    fn read_stat(pid: u32) -> Option<(u32, u64)> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        parse_stat(&stat)
    }

    /// Parses `ppid` and `utime + stime` out of a `/proc/<pid>/stat` line.
    /// The command name sits in parentheses and may itself hold spaces or
    /// parentheses, so fields are counted from its last `)`.
    pub(super) fn parse_stat(stat: &str) -> Option<(u32, u64)> {
        let rest = &stat[stat.rfind(')')? + 1..];
        let fields: Vec<&str> = rest.split_whitespace().collect();
        let ppid = fields.get(1)?.parse().ok()?;
        let utime: u64 = fields.get(11)?.parse().ok()?;
        let stime: u64 = fields.get(12)?.parse().ok()?;
        Some((ppid, utime + stime))
    }

    fn ticks_to_duration(ticks: u64) -> Duration {
        // SAFETY: sysconf has no preconditions.
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        let hz = if hz > 0 { hz as u64 } else { 100 };
        Duration::from_nanos(ticks.saturating_mul(1_000_000_000) / hz)
    }

    pub fn host_memory_bytes() -> Option<u64> {
        let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
        let kib: u64 = meminfo
            .lines()
            .find_map(|line| line.strip_prefix("MemTotal:"))?
            .trim()
            .strip_suffix("kB")?
            .trim()
            .parse()
            .ok()?;
        Some(kib * 1024)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod imp {
    use std::time::Duration;

    pub fn tree_cpu_time(_root: u32) -> Option<Duration> {
        None
    }

    pub fn host_memory_bytes() -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: f64) -> Duration {
        Duration::from_secs_f64(s)
    }

    #[test]
    fn no_pids_is_zero_percent() {
        let mut sampler = CpuSampler::new();
        assert_eq!(sampler.sample(&[], Instant::now(), &|_| None), Some(0.0));
    }

    #[test]
    fn the_first_reading_is_unknown() {
        let mut sampler = CpuSampler::new();
        assert_eq!(
            sampler.sample(&[7], Instant::now(), &|_| Some(secs(3.0))),
            None
        );
    }

    #[test]
    fn percent_is_cpu_time_over_wall_time_summed_across_pids() {
        let mut sampler = CpuSampler::new();
        let start = Instant::now();
        sampler.sample(&[1, 2], start, &|_| Some(secs(10.0)));

        let later = start + secs(2.0);
        let pct = sampler
            .sample(&[1, 2], later, &|pid| {
                Some(secs(if pid == 1 { 13.0 } else { 11.0 }))
            })
            .unwrap();

        // pid 1: 3s over 2s = 150%; pid 2: 1s over 2s = 50%.
        assert!((pct - 200.0).abs() < 0.01, "got {pct}");
    }

    #[test]
    fn a_new_pid_is_left_out_until_its_second_reading() {
        let mut sampler = CpuSampler::new();
        let start = Instant::now();
        sampler.sample(&[1], start, &|_| Some(secs(0.0)));

        let pct = sampler
            .sample(&[1, 2], start + secs(1.0), &|pid| {
                Some(secs(if pid == 1 { 0.5 } else { 99.0 }))
            })
            .unwrap();

        assert!((pct - 50.0).abs() < 0.01, "got {pct}");
    }

    #[test]
    fn a_tree_whose_cpu_time_drops_counts_as_zero() {
        let mut sampler = CpuSampler::new();
        let start = Instant::now();
        sampler.sample(&[1], start, &|_| Some(secs(5.0)));

        let pct = sampler.sample(&[1], start + secs(1.0), &|_| Some(secs(4.0)));

        assert_eq!(pct, Some(0.0));
    }

    #[test]
    fn unreadable_pids_are_unknown() {
        let mut sampler = CpuSampler::new();
        let start = Instant::now();
        sampler.sample(&[1], start, &|_| None);
        assert_eq!(sampler.sample(&[1], start + secs(1.0), &|_| None), None);
    }

    #[test]
    fn a_dropped_pid_is_forgotten() {
        let mut sampler = CpuSampler::new();
        let start = Instant::now();
        sampler.sample(&[1], start, &|_| Some(secs(0.0)));
        sampler.sample(&[], start + secs(1.0), &|_| Some(secs(0.0)));

        // Back again, pid 1 has no previous reading.
        assert_eq!(
            sampler.sample(&[1], start + secs(2.0), &|_| Some(secs(1.0))),
            None
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_stat_counts_fields_from_the_last_paren() {
        let stat = "42 (a (weird) name) S 7 42 42 0 -1 4194560 100 0 0 0 30 12 0 0 20 0 1 0";
        assert_eq!(imp::parse_stat(stat), Some((7, 42)));
    }

    #[test]
    fn host_cores_is_at_least_one() {
        assert!(host_cores() >= 1);
    }

    #[test]
    fn a_dead_root_has_no_cpu_time() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert_eq!(tree_cpu_time(pid), None);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn host_memory_is_readable() {
        assert!(host_memory_bytes().unwrap() > 0);
    }

    /// A shell whose child spins must show more CPU time than the shell
    /// alone could account for.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn tree_cpu_time_includes_a_busy_child() {
        let mut root = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("(while :; do :; done) & wait")
            .spawn()
            .unwrap();
        let pid = root.id();

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = Duration::ZERO;
        while Instant::now() < deadline {
            seen = tree_cpu_time(pid).unwrap_or_default();
            if seen >= Duration::from_millis(300) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        let _ = std::process::Command::new("pkill")
            .args(["-P", &pid.to_string()])
            .status();
        let _ = root.kill();
        let _ = root.wait();

        assert!(
            seen >= Duration::from_millis(300),
            "tree cpu time {seen:?} should include the spinning child"
        );
    }
}
