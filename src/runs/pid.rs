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

/// Sends `SIGKILL` to `pid`, ignoring failure (the process may already be
/// gone). Hibernation's stop signal (GitHub issue #64): unlike `SIGTERM`/
/// `SIGHUP`, a killed agent gets no chance to run its `SessionEnd` hook or
/// exit handler, which would otherwise finish the run it was just
/// hibernated from.
pub fn kill_pid(pid: u32) {
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
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

    #[test]
    fn kill_pid_terminates_a_running_child_without_letting_it_exit_cleanly() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("failed to spawn `sleep`");

        kill_pid(child.id());

        let status = child.wait().expect("failed to wait on child");
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(libc::SIGKILL));
    }
}
