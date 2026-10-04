//! Archive-then-kill for a ticket's `tm-<scope>-<key>` tmux session (GitHub
//! issue #78).
//!
//! One session per ticket is the ticket's browsable action history, so a
//! finished ticket's session is worth keeping until the ticket is done — and
//! then it is clutter. [`archive_and_kill`] is the shared cleanup both `tm
//! merge` (after a successful merge) and `tm work clean` run: capture every
//! window's scrollback to a plain file under the archive root, record one
//! `session_archives` row per window in `runs.db`
//! ([`RunStore::record_session_archive`]), and only then kill the session.
//!
//! Best-effort throughout, and conservative: anything that would make the
//! kill unsafe or lose history leaves the session running and prints a
//! `warning:` line with the manual command instead. That covers a capture
//! or write failure, a session hosting a live run (the same
//! [`run_is_live`] predicate ADR 0005's `live-run` tier uses), the session
//! the caller itself is running inside, and having no runs store to record
//! the archive in. See `docs/decisions/0009-session-scrollback-archive.md`.
//!
//! `capture-pane` only sees what is still inside tmux's `history-limit`, so
//! an archive is best-effort history, not a full transcript; an agent
//! window's own transcript (via the run's recorded `session_id`) stays the
//! authoritative record.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::runs::{NewSessionArchive, RunStore};
use crate::work::kill_safety::{run_hosted_in, run_is_live};
use crate::work::naming::ticket_session_name;
use crate::work::tmux::{TmuxOps, session_window_names};

/// What [`archive_and_kill`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionGcOutcome {
    /// The ticket had no session: nothing to do.
    NoSession,
    /// Every window was archived (these files, in window order) and the
    /// session was killed.
    Killed {
        /// One scrollback file per window.
        archived: Vec<PathBuf>,
    },
    /// The session was left running; a `warning:` line said why.
    Skipped,
}

/// What [`archive_and_kill`] needs.
pub struct SessionGcDeps<'a> {
    /// `tmux` operations (real or fake).
    pub tmux: &'a dyn TmuxOps,
    /// The runs store: checked for live runs hosted in the session, and
    /// where archive rows are recorded. `None` skips the kill (with a
    /// warning), since the archive would be unrecorded.
    pub store: Option<&'a RunStore>,
    /// Directory scrollback files go under, as
    /// `<archive_root>/<slug>/<ticket>/<timestamp>-<window>.log`;
    /// conventionally `<state_dir>/archive` (see [`archive_root`]).
    pub archive_root: &'a Path,
    /// Process liveness probe for a run's recorded pid
    /// ([`crate::runs::pid::pid_alive`] in production).
    pub pid_alive: &'a dyn Fn(u32) -> bool,
    /// Seconds since the Unix epoch, stamped into archive file names.
    pub now_unix_secs: i64,
}

/// The conventional archive root under a tm state directory:
/// `<state_dir>/archive`.
pub fn archive_root(state_dir: &Path) -> PathBuf {
    state_dir.join("archive")
}

/// Archive every window of `ticket`'s session, then kill it. See the
/// module docs for when the kill is skipped instead.
///
/// `scope` is the ticket's [`crate::config::BackendIdentity::scope`] (what
/// archive rows are recorded under) and `slug` its
/// [`crate::config::BackendIdentity::session_slug`] (what the session name
/// and archive directory are built from).
///
/// Only a failure to write to `out` is an error; every tmux, filesystem
/// and store failure becomes a `warning:` line and
/// [`SessionGcOutcome::Skipped`].
pub fn archive_and_kill(
    deps: &SessionGcDeps<'_>,
    scope: &str,
    slug: &str,
    ticket: &str,
    out: &mut dyn Write,
) -> std::io::Result<SessionGcOutcome> {
    let session = ticket_session_name(slug, ticket);
    let kill_hint = format!("tmux kill-session -t {session}");

    match deps.tmux.has_session(&session) {
        Ok(true) => {}
        Ok(false) => return Ok(SessionGcOutcome::NoSession),
        Err(err) => {
            writeln!(
                out,
                "warning: could not check for tmux session {session}: {err}; left it alone (run: {kill_hint})"
            )?;
            return Ok(SessionGcOutcome::Skipped);
        }
    }

    // Like the cwd-inside-worktree guard: killing the session tm is running
    // in would kill tm mid-flow. An error here proves nothing, so skip too.
    match deps.tmux.current_session_name() {
        Ok(Some(current)) if current == session => {
            writeln!(
                out,
                "warning: left tmux session {session} running; tm is running inside it (run: {kill_hint} once you leave it)"
            )?;
            return Ok(SessionGcOutcome::Skipped);
        }
        Ok(_) => {}
        Err(err) => {
            writeln!(
                out,
                "warning: left tmux session {session} running; could not tell whether tm is inside it: {err} (run: {kill_hint})"
            )?;
            return Ok(SessionGcOutcome::Skipped);
        }
    }

    let Some(store) = deps.store else {
        writeln!(
            out,
            "warning: left tmux session {session} running; no runs store to record its archive in (run: {kill_hint})"
        )?;
        return Ok(SessionGcOutcome::Skipped);
    };

    // ADR 0005's `live-run` tier: never kill a session with work in flight.
    let runs = match store.all_runs() {
        Ok(runs) => runs,
        Err(err) => {
            writeln!(
                out,
                "warning: left tmux session {session} running; could not read runs: {err} (run: {kill_hint})"
            )?;
            return Ok(SessionGcOutcome::Skipped);
        }
    };
    if let Some(live) = runs
        .iter()
        .find(|run| run_hosted_in(run, &session) && run_is_live(run, deps.pid_alive))
    {
        writeln!(
            out,
            "warning: left tmux session {session} running; run {} ({} {}) is still {} in it (run: {kill_hint} once it finishes)",
            live.id,
            live.kind,
            live.ticket,
            live.status.as_str()
        )?;
        return Ok(SessionGcOutcome::Skipped);
    }

    let windows = match deps.tmux.list_windows() {
        Ok(windows) => session_window_names(&windows, &session),
        Err(err) => {
            writeln!(
                out,
                "warning: left tmux session {session} running; could not list its windows: {err} (run: {kill_hint})"
            )?;
            return Ok(SessionGcOutcome::Skipped);
        }
    };

    // Capture everything before writing anything, so a failure part-way
    // leaves no half-recorded archive for a session that is still alive.
    let mut captured = Vec::with_capacity(windows.len());
    for window in &windows {
        match deps.tmux.capture_pane(&session, window) {
            Ok(text) => captured.push((window.clone(), text)),
            Err(err) => {
                writeln!(
                    out,
                    "warning: left tmux session {session} running; failed to capture {session}:{window} scrollback: {err} (run: {kill_hint} to drop it unarchived)"
                )?;
                return Ok(SessionGcOutcome::Skipped);
            }
        }
    }

    let dir = deps
        .archive_root
        .join(safe_file_component(slug))
        .join(safe_file_component(ticket));
    let stamp = archive_timestamp(deps.now_unix_secs);
    let mut archived = Vec::with_capacity(captured.len());
    if let Err(err) = std::fs::create_dir_all(&dir) {
        writeln!(
            out,
            "warning: left tmux session {session} running; failed to create {}: {err} (run: {kill_hint} to drop it unarchived)",
            dir.display()
        )?;
        return Ok(SessionGcOutcome::Skipped);
    }
    for (window, text) in &captured {
        let path = dir.join(format!("{stamp}-{}.log", safe_file_component(window)));
        if let Err(err) = std::fs::write(&path, text) {
            writeln!(
                out,
                "warning: left tmux session {session} running; failed to write {}: {err} (run: {kill_hint} to drop it unarchived)",
                path.display()
            )?;
            return Ok(SessionGcOutcome::Skipped);
        }
        archived.push(path);
    }

    for ((window, _), path) in captured.iter().zip(&archived) {
        let row = NewSessionArchive {
            scope: scope.to_string(),
            ticket: ticket.to_string(),
            session: session.clone(),
            window: window.clone(),
            path: path.to_string_lossy().into_owned(),
        };
        if let Err(err) = store.record_session_archive(&row) {
            writeln!(
                out,
                "warning: left tmux session {session} running; failed to record its archive: {err} (files under {}; run: {kill_hint})",
                dir.display()
            )?;
            return Ok(SessionGcOutcome::Skipped);
        }
    }
    if !archived.is_empty() {
        writeln!(
            out,
            "archived {} window(s) of {session} to {}",
            archived.len(),
            dir.display()
        )?;
    }

    match deps.tmux.kill_session(&session) {
        Ok(()) => {
            writeln!(out, "killed tmux session {session}")?;
            Ok(SessionGcOutcome::Killed { archived })
        }
        Err(err) => {
            writeln!(
                out,
                "warning: failed to kill tmux session {session}: {err} (its scrollback is archived; run: {kill_hint})"
            )?;
            Ok(SessionGcOutcome::Skipped)
        }
    }
}

/// `raw` with every character outside `[A-Za-z0-9._-]` replaced by `_`, so
/// a window name (manual windows can be called anything) or ticket key is
/// safe as one path component.
fn safe_file_component(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// `unix_secs` as a UTC `YYYYMMDD-HHMMSS` stamp
/// ([`crate::work::naming::format_timestamp`]'s shape), so a ticket's
/// archive files sort chronologically by name.
fn archive_timestamp(unix_secs: i64) -> String {
    // SAFETY: `gmtime` takes a valid `time_t` pointer (a local, non-null
    // stack value) and returns a pointer into a `libc`-owned static `tm`
    // struct, copied out by value below before any other libc call can
    // overwrite it — no reference escapes this function.
    let tm = unsafe {
        let t: libc::time_t = unix_secs as libc::time_t;
        let tm_ptr = libc::gmtime(&t);
        if tm_ptr.is_null() {
            return format!("unix{unix_secs}");
        }
        *tm_ptr
    };
    crate::work::naming::format_timestamp(
        tm.tm_year + 1900,
        (tm.tm_mon + 1) as u32,
        tm.tm_mday as u32,
        tm.tm_hour as u32,
        tm.tm_min as u32,
        tm.tm_sec as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runs::StartRun;
    use crate::work::tmux::{FakeTmuxOps, TmuxCall, TmuxError, TmuxWindow};
    use tempfile::TempDir;

    const SESSION: &str = "tm-acme-gh-7";

    fn window(name: &str) -> TmuxWindow {
        TmuxWindow {
            session: SESSION.to_string(),
            name: name.to_string(),
            dead: false,
        }
    }

    /// A tmux fake with `SESSION` alive and holding `work` and `shell`.
    fn live_session() -> FakeTmuxOps {
        FakeTmuxOps::new()
            .with_has_session(Ok(true))
            .with_list_windows(Ok(vec![
                window("work"),
                window("shell"),
                TmuxWindow {
                    session: "elsewhere".to_string(),
                    name: "work".to_string(),
                    dead: false,
                },
            ]))
    }

    struct Env {
        tmp: TempDir,
        store: RunStore,
    }

    impl Env {
        fn new() -> Self {
            let tmp = TempDir::new().unwrap();
            let store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
            Env { tmp, store }
        }

        fn root(&self) -> PathBuf {
            self.tmp.path().join("archive")
        }

        fn deps<'a>(&'a self, tmux: &'a FakeTmuxOps, root: &'a Path) -> SessionGcDeps<'a> {
            SessionGcDeps {
                tmux,
                store: Some(&self.store),
                archive_root: root,
                pid_alive: &|_| false,
                now_unix_secs: 1_790_000_000,
            }
        }

        fn start_run(&self, pid: Option<u32>) -> i64 {
            self.store
                .start_run(&StartRun {
                    scope: "github:acme".to_string(),
                    ticket: "GH-7".to_string(),
                    lane: "lane".to_string(),
                    worktree: "/wt".to_string(),
                    branch: None,
                    pid,
                    kind: "lane".to_string(),
                    log_path: None,
                })
                .unwrap()
        }
    }

    fn gc(deps: &SessionGcDeps<'_>) -> (SessionGcOutcome, String) {
        let mut out = Vec::new();
        let outcome = archive_and_kill(deps, "github:acme", "acme", "GH-7", &mut out).unwrap();
        (outcome, String::from_utf8(out).unwrap())
    }

    fn capture_and_kill_calls(tmux: &FakeTmuxOps) -> Vec<TmuxCall> {
        tmux.calls()
            .into_iter()
            .filter(|c| matches!(c, TmuxCall::CapturePane { .. } | TmuxCall::KillSession(_)))
            .collect()
    }

    #[test]
    fn captures_every_window_before_killing_and_records_each_archive() {
        let env = Env::new();
        let tmux = live_session();
        let root = env.root();

        let (outcome, printed) = gc(&env.deps(&tmux, &root));

        assert_eq!(
            capture_and_kill_calls(&tmux),
            vec![
                TmuxCall::CapturePane {
                    name: SESSION.to_string(),
                    window: "work".to_string()
                },
                TmuxCall::CapturePane {
                    name: SESSION.to_string(),
                    window: "shell".to_string()
                },
                TmuxCall::KillSession(SESSION.to_string()),
            ]
        );
        let SessionGcOutcome::Killed { archived } = outcome else {
            panic!("expected Killed, got {outcome:?}: {printed}");
        };
        assert_eq!(archived.len(), 2);
        assert!(archived[0].starts_with(root.join("acme").join("GH-7")));
        assert!(
            archived[0]
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("-work.log")
        );
        assert_eq!(
            std::fs::read_to_string(&archived[0]).unwrap(),
            format!("{SESSION}:work scrollback\n")
        );

        let rows = env
            .store
            .session_archives_for_ticket(Some("github:acme"), "GH-7")
            .unwrap();
        assert_eq!(
            rows.iter().map(|r| r.window.as_str()).collect::<Vec<_>>(),
            vec!["work", "shell"]
        );
        assert_eq!(rows[0].session, SESSION);
        assert_eq!(rows[0].path, archived[0].to_string_lossy());
        assert!(
            printed.contains(&format!("killed tmux session {SESSION}")),
            "{printed}"
        );
    }

    #[test]
    fn no_session_is_a_quiet_no_op() {
        let env = Env::new();
        let tmux = FakeTmuxOps::new().with_has_session(Ok(false));
        let root = env.root();

        let (outcome, printed) = gc(&env.deps(&tmux, &root));

        assert_eq!(outcome, SessionGcOutcome::NoSession);
        assert_eq!(printed, "");
        assert_eq!(
            tmux.calls(),
            vec![TmuxCall::HasSession(SESSION.to_string())]
        );
    }

    #[test]
    fn a_capture_failure_leaves_the_session_alive_and_records_nothing() {
        let env = Env::new();
        let tmux = live_session().with_capture_pane_failure(
            "shell",
            TmuxError::Command {
                command: "tmux capture-pane".to_string(),
                exit_code: Some(1),
                stderr: "boom".to_string(),
            },
        );
        let root = env.root();

        let (outcome, printed) = gc(&env.deps(&tmux, &root));

        assert_eq!(outcome, SessionGcOutcome::Skipped);
        assert!(
            !tmux
                .calls()
                .contains(&TmuxCall::KillSession(SESSION.to_string()))
        );
        assert!(printed.contains("warning:"), "{printed}");
        assert!(
            printed.contains(&format!("tmux kill-session -t {SESSION}")),
            "{printed}"
        );
        assert!(
            env.store
                .session_archives_for_ticket(None, "GH-7")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_running_run_hosted_in_the_session_blocks_the_kill() {
        let env = Env::new();
        let id = env.start_run(None);
        let tmux = live_session();
        let root = env.root();

        let (outcome, printed) = gc(&env.deps(&tmux, &root));

        assert_eq!(outcome, SessionGcOutcome::Skipped);
        assert!(capture_and_kill_calls(&tmux).is_empty(), "{printed}");
        assert!(printed.contains("warning:"), "{printed}");
        assert!(printed.contains(&format!("run {id}")), "{printed}");
    }

    #[test]
    fn a_hibernated_run_hosted_in_the_session_blocks_the_kill() {
        let env = Env::new();
        let id = env.start_run(Some(1));
        assert!(env.store.hibernate_run(id).unwrap());
        let tmux = live_session();
        let root = env.root();

        let (outcome, _) = gc(&env.deps(&tmux, &root));

        assert_eq!(outcome, SessionGcOutcome::Skipped);
    }

    #[test]
    fn a_running_row_whose_pid_is_dead_does_not_block_the_kill() {
        let env = Env::new();
        env.start_run(Some(4242));
        let tmux = live_session();
        let root = env.root();

        let (outcome, printed) = gc(&env.deps(&tmux, &root));

        assert!(
            matches!(outcome, SessionGcOutcome::Killed { .. }),
            "{printed}"
        );
    }

    #[test]
    fn the_session_the_caller_is_running_inside_is_never_killed() {
        let env = Env::new();
        let tmux = live_session().with_current_session_name(Ok(Some(SESSION.to_string())));
        let root = env.root();

        let (outcome, printed) = gc(&env.deps(&tmux, &root));

        assert_eq!(outcome, SessionGcOutcome::Skipped);
        assert!(capture_and_kill_calls(&tmux).is_empty());
        assert!(printed.contains("inside it"), "{printed}");
    }

    #[test]
    fn no_runs_store_skips_the_kill() {
        let env = Env::new();
        let tmux = live_session();
        let root = env.root();
        let deps = SessionGcDeps {
            store: None,
            ..env.deps(&tmux, &root)
        };

        let (outcome, printed) = gc(&deps);

        assert_eq!(outcome, SessionGcOutcome::Skipped);
        assert!(
            !tmux
                .calls()
                .contains(&TmuxCall::KillSession(SESSION.to_string()))
        );
        assert!(printed.contains("warning:"), "{printed}");
    }

    #[test]
    fn window_names_are_made_safe_for_file_names() {
        assert_eq!(safe_file_component("work"), "work");
        assert_eq!(safe_file_component("audit-2"), "audit-2");
        assert_eq!(safe_file_component("a/b c:d"), "a_b_c_d");
    }

    #[test]
    fn archive_timestamps_are_utc_and_sortable() {
        assert_eq!(archive_timestamp(0), "19700101-000000");
        assert_eq!(archive_timestamp(1_790_000_000), "20260921-141320");
    }
}
