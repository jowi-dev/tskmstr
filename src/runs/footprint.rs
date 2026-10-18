//! Memory measurement for running lanes (GitHub issue #66): the footprint
//! of a run's whole process tree, and the kernel's memory-pressure verdict.
//!
//! **Footprint** is a process's real memory cost. On macOS it is the
//! number Activity Monitor and `footprint -p` show: it counts compressed and
//! swapped-out pages, which resident set size leaves out, so it stays honest
//! on a machine that is already under pressure. On Linux (GitHub issue
//! #108) it is the proportional set size from `/proc/<pid>/smaps_rollup`,
//! which splits shared pages among the processes mapping them, so a tree of
//! `rustc`s sharing libraries isn't counted twice. Where the rollup can't be
//! read it falls back to resident set size from `/proc/<pid>/statm`.
//!
//! **Pressure** comes from `kern.memorystatus_vm_pressure_level` on macOS.
//! On Linux it comes from the `some avg10` stall share in
//! `/proc/pressure/memory` (PSI), mapped by [`PSI_WARN_AVG10`] and
//! [`PSI_CRITICAL_AVG10`]. Kernels without PSI fall back to the share of
//! `MemTotal` still `MemAvailable` in `/proc/meminfo`, mapped by
//! [`MEMINFO_WARN_AVAILABLE_PCT`] and [`MEMINFO_CRITICAL_AVAILABLE_PCT`].
//!
//! On other platforms both probes report "unknown" ([`tree_footprint`]
//! returns `None`, [`memory_pressure`] returns [`MemoryPressure::Unknown`]),
//! which callers treat as "no data" rather than as an error.

/// The kernel's memory-pressure verdict: from
/// `kern.memorystatus_vm_pressure_level` on macOS, from PSI or
/// `/proc/meminfo` on Linux.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryPressure {
    /// No pressure.
    Normal,
    /// The kernel is compressing and swapping hard.
    Warn,
    /// The kernel is close to killing processes.
    Critical,
    /// The level could not be read (unsupported platform, or every source
    /// failed).
    Unknown,
}

impl MemoryPressure {
    /// Maps the raw sysctl value: `1` normal, `2` warn, `4` critical.
    pub fn from_level(level: i32) -> Self {
        match level {
            1 => MemoryPressure::Normal,
            2 => MemoryPressure::Warn,
            4 => MemoryPressure::Critical,
            _ => MemoryPressure::Unknown,
        }
    }

    /// Whether this level should stop a new lane launch.
    pub fn blocks_launch(self) -> bool {
        matches!(self, MemoryPressure::Warn | MemoryPressure::Critical)
    }

    /// Lowercase name used in messages.
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryPressure::Normal => "normal",
            MemoryPressure::Warn => "warn",
            MemoryPressure::Critical => "critical",
            MemoryPressure::Unknown => "unknown",
        }
    }
}

/// Bytes in one mebibyte.
pub const MIB: u64 = 1024 * 1024;
/// Bytes in one gibibyte. Config values like `memory_budget_gb` are read in
/// these units, the same ones Activity Monitor labels "GB".
pub const GIB: u64 = 1024 * MIB;

/// Renders a byte count for display: whole megabytes below a gigabyte
/// (`850 MB`), one decimal place from there up (`2.5 GB`).
pub fn format_bytes(bytes: u64) -> String {
    if bytes < GIB {
        format!("{} MB", bytes / MIB)
    } else {
        format!("{:.1} GB", bytes as f64 / GIB as f64)
    }
}

/// Returns the summed footprint, in bytes, of `root` and every process
/// descended from it, or `None` when `root` is not running (or on a
/// platform with no footprint API).
///
/// A lane is a process tree, not one process: the agent spawns `cargo`,
/// `rustc`, language servers, and test binaries, and most of a lane's
/// memory spike comes from those children. Descendants that exit between
/// listing and measuring are skipped. A child that outlives its parent is
/// reparented to `launchd` (or `init` or a subreaper on Linux) and drops
/// out of the tree.
pub fn tree_footprint(root: u32) -> Option<u64> {
    imp::tree_footprint(root)
}

/// Reads the kernel's current memory-pressure level.
pub fn memory_pressure() -> MemoryPressure {
    imp::memory_pressure()
}

/// The direct children of a pid, shared with [`super::cpu`]'s tree walk.
#[cfg(target_os = "macos")]
pub(crate) use imp::child_pids;

#[cfg(target_os = "macos")]
mod imp {
    use super::MemoryPressure;

    pub fn tree_footprint(root: u32) -> Option<u64> {
        let mut total = process_footprint(root)?;
        let mut pending = child_pids(root);
        while let Some(pid) = pending.pop() {
            if let Some(bytes) = process_footprint(pid) {
                total += bytes;
                pending.extend(child_pids(pid));
            }
        }
        Some(total)
    }

    fn process_footprint(pid: u32) -> Option<u64> {
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
        Some(unsafe { info.assume_init() }.ri_phys_footprint)
    }

    pub fn child_pids(pid: u32) -> Vec<u32> {
        let mut capacity = 64usize;
        loop {
            let mut buf: Vec<libc::pid_t> = vec![0; capacity];
            let bytes = (capacity * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
            // SAFETY: `buf` is a writable buffer of exactly `bytes` bytes.
            let count = unsafe {
                libc::proc_listchildpids(pid as libc::pid_t, buf.as_mut_ptr().cast(), bytes)
            };
            if count <= 0 {
                return Vec::new();
            }
            let count = count as usize;
            // A full buffer may have been truncated; retry with more room.
            if count < capacity {
                buf.truncate(count);
                return buf
                    .into_iter()
                    .filter(|&p| p > 0)
                    .map(|p| p as u32)
                    .collect();
            }
            capacity *= 4;
        }
    }

    pub fn memory_pressure() -> MemoryPressure {
        let mut level: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>();
        // SAFETY: the name is a NUL-terminated C string, and `level`/`len`
        // describe a writable `c_int` of the size the sysctl returns.
        let rc = unsafe {
            libc::sysctlbyname(
                c"kern.memorystatus_vm_pressure_level".as_ptr(),
                (&mut level as *mut libc::c_int).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return MemoryPressure::Unknown;
        }
        MemoryPressure::from_level(level)
    }
}

/// PSI `some avg10` (percent of the last ten seconds in which at least one
/// task stalled waiting on memory) at or above which Linux reports
/// [`MemoryPressure::Warn`]. A tenth of wall time lost to reclaim is a
/// machine already struggling to give a new lane room.
pub const PSI_WARN_AVG10: f64 = 10.0;

/// PSI `some avg10` at or above which Linux reports
/// [`MemoryPressure::Critical`]: tasks stall on memory for 40% of the time,
/// the territory where the OOM killer or systemd-oomd steps in.
pub const PSI_CRITICAL_AVG10: f64 = 40.0;

/// Percent of `MemTotal` still `MemAvailable` below which the meminfo
/// fallback reports [`MemoryPressure::Warn`].
pub const MEMINFO_WARN_AVAILABLE_PCT: u64 = 10;

/// Percent of `MemTotal` still `MemAvailable` below which the meminfo
/// fallback reports [`MemoryPressure::Critical`].
pub const MEMINFO_CRITICAL_AVAILABLE_PCT: u64 = 5;

/// Reads `some avg10` out of `/proc/pressure/memory`.
#[cfg(any(target_os = "linux", test))]
fn parse_psi_some_avg10(psi: &str) -> Option<f64> {
    psi.lines()
        .find_map(|line| line.strip_prefix("some "))?
        .split_whitespace()
        .find_map(|field| field.strip_prefix("avg10="))?
        .parse()
        .ok()
}

/// Maps a PSI `some avg10` percentage to a pressure level using
/// [`PSI_WARN_AVG10`] and [`PSI_CRITICAL_AVG10`].
#[cfg(any(target_os = "linux", test))]
fn pressure_from_psi(avg10: f64) -> MemoryPressure {
    if avg10 >= PSI_CRITICAL_AVG10 {
        MemoryPressure::Critical
    } else if avg10 >= PSI_WARN_AVG10 {
        MemoryPressure::Warn
    } else {
        MemoryPressure::Normal
    }
}

/// Reads a `<Key>:   <n> kB` line's value, in bytes.
#[cfg(any(target_os = "linux", test))]
fn kib_field(text: &str, key: &str) -> Option<u64> {
    let kib: u64 = text
        .lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix(':'))?
        .trim()
        .strip_suffix("kB")?
        .trim()
        .parse()
        .ok()?;
    Some(kib * 1024)
}

/// Reads `(MemAvailable, MemTotal)` in bytes out of `/proc/meminfo`. `None`
/// when either is missing or the total is zero.
#[cfg(any(target_os = "linux", test))]
fn parse_meminfo(meminfo: &str) -> Option<(u64, u64)> {
    let available = kib_field(meminfo, "MemAvailable")?;
    let total = kib_field(meminfo, "MemTotal")?;
    (total > 0).then_some((available, total))
}

/// Maps the share of memory still available to a pressure level using
/// [`MEMINFO_WARN_AVAILABLE_PCT`] and [`MEMINFO_CRITICAL_AVAILABLE_PCT`].
#[cfg(any(target_os = "linux", test))]
fn pressure_from_available(available: u64, total: u64) -> MemoryPressure {
    let scaled = available.saturating_mul(100);
    if scaled < total.saturating_mul(MEMINFO_CRITICAL_AVAILABLE_PCT) {
        MemoryPressure::Critical
    } else if scaled < total.saturating_mul(MEMINFO_WARN_AVAILABLE_PCT) {
        MemoryPressure::Warn
    } else {
        MemoryPressure::Normal
    }
}

/// Reads the `Pss:` total, in bytes, out of `/proc/<pid>/smaps_rollup`.
#[cfg(any(target_os = "linux", test))]
fn parse_smaps_rollup_pss(rollup: &str) -> Option<u64> {
    kib_field(rollup, "Pss")
}

/// Reads the resident page count (second field) out of `/proc/<pid>/statm`.
#[cfg(any(target_os = "linux", test))]
fn parse_statm_rss_pages(statm: &str) -> Option<u64> {
    statm.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(target_os = "linux")]
mod imp {
    use super::MemoryPressure;

    /// Walks the tree from one `/proc` scan (shared with
    /// [`crate::runs::cpu`]), summing each process's footprint.
    pub fn tree_footprint(root: u32) -> Option<u64> {
        let mut total = process_footprint(root)?;
        let children = crate::runs::cpu::children_by_parent().unwrap_or_default();
        let mut pending = vec![root];
        while let Some(pid) = pending.pop() {
            for &child in children.get(&pid).into_iter().flatten() {
                if let Some(bytes) = process_footprint(child) {
                    total += bytes;
                    pending.push(child);
                }
            }
        }
        Some(total)
    }

    /// PSS from `smaps_rollup` (shared pages split among their users, so a
    /// tree of `rustc`s sharing libraries isn't double counted), falling
    /// back to RSS from `statm` where the rollup is unreadable (old kernels,
    /// or ptrace restrictions).
    fn process_footprint(pid: u32) -> Option<u64> {
        if let Some(pss) = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))
            .ok()
            .and_then(|rollup| super::parse_smaps_rollup_pss(&rollup))
        {
            return Some(pss);
        }
        let statm = std::fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
        let pages = super::parse_statm_rss_pages(&statm)?;
        // SAFETY: sysconf has no preconditions.
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        let page_size = if page_size > 0 {
            page_size as u64
        } else {
            4096
        };
        Some(pages * page_size)
    }

    /// PSI first; `MemAvailable`/`MemTotal` where PSI is absent (kernels
    /// without `CONFIG_PSI`, or booted with `psi=0`).
    pub fn memory_pressure() -> MemoryPressure {
        if let Some(avg10) = std::fs::read_to_string("/proc/pressure/memory")
            .ok()
            .and_then(|psi| super::parse_psi_some_avg10(&psi))
        {
            return super::pressure_from_psi(avg10);
        }
        std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|meminfo| super::parse_meminfo(&meminfo))
            .map_or(MemoryPressure::Unknown, |(available, total)| {
                super::pressure_from_available(available, total)
            })
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod imp {
    use super::MemoryPressure;

    pub fn tree_footprint(_root: u32) -> Option<u64> {
        None
    }

    pub fn memory_pressure() -> MemoryPressure {
        MemoryPressure::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressure_levels_map_from_the_sysctl_values() {
        assert_eq!(MemoryPressure::from_level(1), MemoryPressure::Normal);
        assert_eq!(MemoryPressure::from_level(2), MemoryPressure::Warn);
        assert_eq!(MemoryPressure::from_level(4), MemoryPressure::Critical);
        assert_eq!(MemoryPressure::from_level(0), MemoryPressure::Unknown);
    }

    #[test]
    fn only_warn_and_critical_block_a_launch() {
        assert!(!MemoryPressure::Normal.blocks_launch());
        assert!(MemoryPressure::Warn.blocks_launch());
        assert!(MemoryPressure::Critical.blocks_launch());
        assert!(!MemoryPressure::Unknown.blocks_launch());
    }

    #[test]
    fn format_bytes_uses_mb_below_a_gigabyte_and_gb_above() {
        assert_eq!(format_bytes(0), "0 MB");
        assert_eq!(format_bytes(850 * MIB), "850 MB");
        assert_eq!(format_bytes(1024 * MIB), "1.0 GB");
        assert_eq!(format_bytes(2560 * MIB), "2.5 GB");
    }

    const PSI: &str = "some avg10=12.50 avg60=3.00 avg300=1.00 total=154046\n\
                       full avg10=4.00 avg60=1.00 avg300=0.20 total=144424\n";

    #[test]
    fn psi_some_avg10_is_parsed() {
        assert_eq!(parse_psi_some_avg10(PSI), Some(12.5));
    }

    #[test]
    fn malformed_psi_has_no_avg10() {
        assert_eq!(parse_psi_some_avg10(""), None);
        assert_eq!(parse_psi_some_avg10("full avg10=4.00 avg60=1.00"), None);
        assert_eq!(parse_psi_some_avg10("some avg10=lots avg60=1.00"), None);
        assert_eq!(parse_psi_some_avg10("some avg60=1.00 total=3"), None);
    }

    #[test]
    fn psi_stall_share_maps_to_pressure_levels() {
        assert_eq!(pressure_from_psi(0.0), MemoryPressure::Normal);
        assert_eq!(
            pressure_from_psi(PSI_WARN_AVG10 - 0.01),
            MemoryPressure::Normal
        );
        assert_eq!(pressure_from_psi(PSI_WARN_AVG10), MemoryPressure::Warn);
        assert_eq!(
            pressure_from_psi(PSI_CRITICAL_AVG10),
            MemoryPressure::Critical
        );
        assert_eq!(pressure_from_psi(100.0), MemoryPressure::Critical);
    }

    const MEMINFO: &str = "MemTotal:       16000000 kB\n\
                           MemFree:         1000000 kB\n\
                           MemAvailable:    4000000 kB\n\
                           Buffers:          100000 kB\n";

    #[test]
    fn meminfo_available_and_total_are_parsed_as_bytes() {
        assert_eq!(
            parse_meminfo(MEMINFO),
            Some((4_000_000 * 1024, 16_000_000 * 1024))
        );
    }

    #[test]
    fn malformed_meminfo_is_none() {
        assert_eq!(parse_meminfo(""), None);
        assert_eq!(parse_meminfo("MemTotal:       16000000 kB\n"), None);
        assert_eq!(parse_meminfo("MemAvailable:    4000000 kB\n"), None);
        assert_eq!(
            parse_meminfo("MemTotal: many kB\nMemAvailable: 4000000 kB\n"),
            None
        );
        assert_eq!(parse_meminfo("MemTotal: 0 kB\nMemAvailable: 0 kB\n"), None);
    }

    #[test]
    fn available_share_maps_to_pressure_levels() {
        assert_eq!(pressure_from_available(50, 100), MemoryPressure::Normal);
        assert_eq!(pressure_from_available(10, 100), MemoryPressure::Normal);
        assert_eq!(pressure_from_available(9, 100), MemoryPressure::Warn);
        assert_eq!(pressure_from_available(5, 100), MemoryPressure::Warn);
        assert_eq!(pressure_from_available(4, 100), MemoryPressure::Critical);
        assert_eq!(pressure_from_available(0, 100), MemoryPressure::Critical);
    }

    #[test]
    fn smaps_rollup_pss_is_parsed_as_bytes() {
        let rollup = "62c6cec4c000-7ffe9473f000 ---p 00000000 00:00 0  [rollup]\n\
                      Rss:                3172 kB\n\
                      Pss:                1939 kB\n\
                      Pss_Anon:            512 kB\n";
        assert_eq!(parse_smaps_rollup_pss(rollup), Some(1939 * 1024));
    }

    #[test]
    fn malformed_smaps_rollup_is_none() {
        assert_eq!(parse_smaps_rollup_pss(""), None);
        assert_eq!(
            parse_smaps_rollup_pss("Rss: 3172 kB\nPss_Anon: 512 kB\n"),
            None
        );
        assert_eq!(parse_smaps_rollup_pss("Pss: lots kB\n"), None);
    }

    #[test]
    fn statm_rss_is_the_second_field_in_pages() {
        assert_eq!(
            parse_statm_rss_pages("2606 772 719 280 0 698 0\n"),
            Some(772)
        );
        assert_eq!(parse_statm_rss_pages("2606"), None);
        assert_eq!(parse_statm_rss_pages("2606 many"), None);
    }

    #[test]
    fn a_dead_root_has_no_footprint() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert_eq!(tree_footprint(pid), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn own_process_has_a_footprint() {
        assert!(tree_footprint(std::process::id()).unwrap() > 0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn pressure_is_readable_on_macos() {
        assert_ne!(memory_pressure(), MemoryPressure::Unknown);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn own_process_tree_has_a_footprint_on_linux() {
        assert!(tree_footprint(std::process::id()).unwrap() > 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pressure_is_readable_on_linux() {
        assert_ne!(memory_pressure(), MemoryPressure::Unknown);
    }

    /// Linux counterpart of the macOS child-allocation test: a shell whose
    /// child holds ~200 MB must measure well above that.
    #[cfg(target_os = "linux")]
    #[test]
    fn tree_footprint_includes_a_child_that_allocates_memory_on_linux() {
        const ALLOC: u64 = 200 * 1024 * 1024;
        let mut root = std::process::Command::new("sh")
            .arg("-c")
            .arg(r#"perl -e '$x = "a" x 209715200; sleep 30' & wait"#)
            .spawn()
            .unwrap();
        let pid = root.id();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut seen = 0;
        while std::time::Instant::now() < deadline {
            seen = tree_footprint(pid).unwrap_or(0);
            if seen > ALLOC {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        let _ = std::process::Command::new("pkill")
            .args(["-P", &pid.to_string()])
            .status();
        let _ = root.kill();
        let _ = root.wait();

        assert!(seen > ALLOC, "tree footprint {seen} should exceed {ALLOC}");
    }

    /// The acceptance test for "peak footprint includes child processes":
    /// a shell whose child allocates ~200 MB must measure well above what
    /// the shell alone costs.
    #[cfg(target_os = "macos")]
    #[test]
    fn tree_footprint_includes_a_child_that_allocates_memory() {
        const ALLOC: u64 = 200 * 1024 * 1024;
        let mut root = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(r#"/usr/bin/perl -e '$x = "a" x 209715200; sleep 30' & wait"#)
            .spawn()
            .unwrap();
        let pid = root.id();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut seen = 0;
        while std::time::Instant::now() < deadline {
            seen = tree_footprint(pid).unwrap_or(0);
            if seen > ALLOC {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let shell_alone = imp_process_footprint_for_test(pid);

        // Kill the whole group: the perl child first, then the shell.
        let _ = std::process::Command::new("/usr/bin/pkill")
            .args(["-P", &pid.to_string()])
            .status();
        let _ = root.kill();
        let _ = root.wait();

        assert!(seen > ALLOC, "tree footprint {seen} should exceed {ALLOC}");
        assert!(
            shell_alone < ALLOC / 4,
            "shell alone measured {shell_alone}"
        );
    }

    #[cfg(target_os = "macos")]
    fn imp_process_footprint_for_test(pid: u32) -> u64 {
        let mut info = std::mem::MaybeUninit::<libc::rusage_info_v2>::zeroed();
        unsafe {
            libc::proc_pid_rusage(
                pid as libc::c_int,
                libc::RUSAGE_INFO_V2,
                info.as_mut_ptr().cast::<libc::rusage_info_t>(),
            );
            info.assume_init().ri_phys_footprint
        }
    }
}
