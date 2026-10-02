//! Memory measurement for running lanes (GitHub issue #66): the footprint
//! of a run's whole process tree, and the kernel's memory-pressure verdict.
//!
//! **Footprint** is the macOS measure of a process's real memory cost. It
//! counts compressed and swapped-out pages, which resident set size leaves
//! out, so it stays honest on a machine that is already under pressure. It
//! is the number Activity Monitor and `footprint -p` show.
//!
//! Both probes are macOS-only. Elsewhere they report "unknown"
//! ([`tree_footprint`] returns `None`, [`memory_pressure`] returns
//! [`MemoryPressure::Unknown`]), which callers treat as "no data" rather
//! than as an error.

/// The kernel's memory-pressure verdict, read from
/// `kern.memorystatus_vm_pressure_level`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryPressure {
    /// No pressure.
    Normal,
    /// The kernel is compressing and swapping hard.
    Warn,
    /// The kernel is close to killing processes.
    Critical,
    /// The level could not be read (non-macOS, or the sysctl failed).
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

/// Returns the summed footprint, in bytes, of `root` and every process
/// descended from it, or `None` when `root` is not running (or on a
/// platform with no footprint API).
///
/// A lane is a process tree, not one process: the agent spawns `cargo`,
/// `rustc`, language servers, and test binaries, and most of a lane's
/// memory spike comes from those children. Descendants that exit between
/// listing and measuring are skipped. A child that outlives its parent is
/// reparented to `launchd` and drops out of the tree.
pub fn tree_footprint(root: u32) -> Option<u64> {
    imp::tree_footprint(root)
}

/// Reads the kernel's current memory-pressure level.
pub fn memory_pressure() -> MemoryPressure {
    imp::memory_pressure()
}

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

    fn child_pids(pid: u32) -> Vec<u32> {
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

#[cfg(not(target_os = "macos"))]
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
