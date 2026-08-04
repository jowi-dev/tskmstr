//! Process liveness check used by [`crate::runs::RunStore::reap`].

/// Returns whether a process with `pid` currently exists, via `kill(pid, 0)`.
///
/// `0` and `EPERM` both mean the process exists (EPERM: owned by another
/// user); `ESRCH` means it does not. Caveat: PIDs are recycled, so a "live"
/// answer may be a different process than the one originally recorded —
/// callers must combine this with another signal (reap requires a stale
/// heartbeat AND a dead pid).
pub fn pid_alive(pid: u32) -> bool {
    match unsafe { libc::kill(pid as libc::pid_t, 0) } {
        0 => true,
        _ => std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_pid_is_alive() {
        assert!(pid_alive(std::process::id()));
    }

    #[test]
    fn a_spawned_and_waited_child_is_dead() {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("failed to spawn `true`");
        let pid = child.id();
        child.wait().expect("failed to wait on child");

        assert!(!pid_alive(pid));
    }
}
