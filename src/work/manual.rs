//! Board-launched manual ticket sessions (`m` on the board).
//!
//! The human escape hatch from the Claude-driven flows: for a ticket small
//! enough that a Claude round trip is slower than hand-editing, the board's
//! `m` key ensures the ticket's tmux session exists with the operator's
//! configured default window layout (`[work.manual].windows`, e.g. code,
//! fish, claude, server) and attaches to it.
//!
//! Unlike audits (`crate::work::audit`) and interactive work/fix runs
//! (`crate::work::interactive`), a manual session is human work: no run row
//! is pre-registered in runs.db and no `TSKMSTR_SESSION_RUN_ID` is exported —
//! there is no driver process to track.
//!
//! The layout is ensured, never rebuilt: each configured window is created
//! only when the session has no live window of that name (per
//! [`has_live_window`]'s action-name matching), so repeated presses are
//! idempotent and the manual windows coexist with whatever audit/work/fix
//! windows already live in the ticket's session.

use std::path::Path;

use thiserror::Error;

use crate::config::{BackendIdentity, ManualConfig};
use crate::work::naming::{expand_tilde, ticket_session_name};
use crate::work::tmux::{
    has_live_window, session_window_names, unique_window_name, TmuxError, TmuxOps,
};

/// Why a manual session could not be ensured.
#[derive(Debug, Error)]
pub enum ManualLaunchError {
    /// `[work.manual]` is missing (or incomplete) in config; the board
    /// surfaces this as a status-line message, not a crash.
    #[error("manual sessions are not configured; set [work.manual].dir and [work.manual].windows")]
    NotConfigured,
    /// A tmux operation failed.
    #[error(transparent)]
    Tmux(#[from] TmuxError),
}

/// What [`ensure_manual_session`] did, for the board's status line.
#[derive(Debug, PartialEq, Eq)]
pub struct ManualOutcome {
    /// The ticket's session name — what the caller attaches to.
    pub session_name: String,
    /// Names of the windows this call actually created, in creation order.
    /// Empty means the layout was already complete (the idempotent re-press).
    pub created_windows: Vec<String>,
}

/// Ensures the ticket's session exists with every `[work.manual]` window,
/// creating only what's missing.
///
/// 1. Errors with [`ManualLaunchError::NotConfigured`] unless `manual_cfg`
///    has both a `dir` and a non-empty `windows` list.
/// 2. Takes one [`TmuxOps::list_windows`] snapshot (the same
///    single-snapshot idiom as `crate::work::audit::launch_audit`) and skips
///    every configured window the session already holds live; a missing
///    window is created with a [`unique_window_name`] suffix if a dead
///    predecessor still occupies the plain name.
/// 3. Creates the session from the first missing window when the ticket has
///    no session yet, and hands focus to the first configured window so the
///    operator lands on it (attach order is the caller's job). A session
///    that already exists keeps its current window selection.
///
/// A window entry with a `command` runs it via tmux's `$SHELL -c` handoff;
/// one without (or with a blank command) is a plain interactive shell.
///
/// `home` resolves a leading `~` in `manual_cfg.dir` via [`expand_tilde`],
/// matching every other `~`-expanding config caller. `identity` scopes the
/// session name per repo (GitHub issue #10), exactly like every other
/// tmux-hosted action.
pub fn ensure_manual_session(
    tmux: &dyn TmuxOps,
    manual_cfg: &ManualConfig,
    home: &Path,
    identity: &BackendIdentity,
    key: &str,
) -> Result<ManualOutcome, ManualLaunchError> {
    let raw_dir = manual_cfg
        .dir
        .as_deref()
        .ok_or(ManualLaunchError::NotConfigured)?;
    if manual_cfg.windows.is_empty() {
        return Err(ManualLaunchError::NotConfigured);
    }
    let dir = expand_tilde(raw_dir, home);
    let dir_str = dir.to_string_lossy().into_owned();

    let session_name = ticket_session_name(&identity.session_slug(), key);
    let windows = tmux.list_windows()?;
    let mut existing = session_window_names(&windows, &session_name);
    let session_exists = !existing.is_empty();

    let mut created_windows = Vec::new();
    for window in &manual_cfg.windows {
        if has_live_window(&windows, &session_name, &window.name) {
            continue;
        }
        let window_name = unique_window_name(&window.name, &existing);
        let command = window
            .command
            .as_deref()
            .filter(|command| !command.trim().is_empty());
        let creating_session = !session_exists && created_windows.is_empty();
        match (creating_session, command) {
            (true, Some(command)) => {
                tmux.new_session_with_command(&session_name, &dir_str, &window_name, &[], command)?
            }
            (true, None) => tmux.new_session(&session_name, &dir_str, &window_name)?,
            (false, Some(command)) => {
                tmux.new_window_with_command(&session_name, &window_name, &dir_str, &[], command)?
            }
            (false, None) => tmux.new_window(&session_name, &window_name, &dir_str)?,
        }
        existing.push(window_name.clone());
        created_windows.push(window_name);
    }

    if !session_exists && !created_windows.is_empty() {
        // A fresh session's focus sits on the last window created; land the
        // operator on the layout's first window instead.
        tmux.select_window(&session_name, &created_windows[0])?;
    }

    Ok(ManualOutcome {
        session_name,
        created_windows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ManualWindow;
    use crate::work::tmux::{FakeTmuxOps, TmuxCall, TmuxWindow};
    use std::path::PathBuf;

    fn identity() -> BackendIdentity {
        BackendIdentity::Jira {
            base_url: "https://x.atlassian.net".to_string(),
            project_key: "PROJ".to_string(),
        }
    }

    fn window_entry(name: &str, command: Option<&str>) -> ManualWindow {
        ManualWindow {
            name: name.to_string(),
            command: command.map(str::to_string),
        }
    }

    fn configured() -> ManualConfig {
        ManualConfig {
            dir: Some("~/Projects/axiom".to_string()),
            windows: vec![
                window_entry("code", Some("nvim")),
                window_entry("fish", None),
                window_entry("server", Some("make server")),
            ],
        }
    }

    fn tmux_window(session: &str, name: &str, dead: bool) -> TmuxWindow {
        TmuxWindow {
            session: session.to_string(),
            name: name.to_string(),
            dead,
        }
    }

    #[test]
    fn errors_not_configured_without_windows() {
        let tmux = FakeTmuxOps::new();
        let cfg = ManualConfig {
            dir: Some("~/Projects/axiom".to_string()),
            windows: vec![],
        };
        let err = ensure_manual_session(&tmux, &cfg, Path::new("/home/u"), &identity(), "PROJ-7")
            .expect_err("should refuse without windows");
        assert!(matches!(err, ManualLaunchError::NotConfigured));
        assert!(
            tmux.calls().is_empty(),
            "a refused launch should not touch tmux"
        );
        assert!(err.to_string().contains("[work.manual]"));
    }

    #[test]
    fn errors_not_configured_without_dir() {
        let tmux = FakeTmuxOps::new();
        let cfg = ManualConfig {
            dir: None,
            windows: vec![window_entry("code", None)],
        };
        let err = ensure_manual_session(&tmux, &cfg, Path::new("/home/u"), &identity(), "PROJ-7")
            .expect_err("should refuse without dir");
        assert!(matches!(err, ManualLaunchError::NotConfigured));
        assert!(tmux.calls().is_empty());
    }

    #[test]
    fn fresh_session_creates_every_window_and_selects_the_first() {
        let tmux = FakeTmuxOps::new();
        let outcome = ensure_manual_session(
            &tmux,
            &configured(),
            Path::new("/home/u"),
            &identity(),
            "PROJ-7",
        )
        .expect("should launch");

        assert_eq!(outcome.session_name, "tm-proj-proj-7");
        assert_eq!(outcome.created_windows, vec!["code", "fish", "server"]);
        assert_eq!(
            tmux.calls(),
            vec![
                TmuxCall::ListWindows,
                TmuxCall::NewSessionWithCommand {
                    name: "tm-proj-proj-7".to_string(),
                    dir: "/home/u/Projects/axiom".to_string(),
                    window_name: "code".to_string(),
                    env: vec![],
                    command: "nvim".to_string(),
                },
                TmuxCall::NewWindow {
                    name: "tm-proj-proj-7".to_string(),
                    window_name: "fish".to_string(),
                    dir: "/home/u/Projects/axiom".to_string(),
                },
                TmuxCall::NewWindowWithCommand {
                    name: "tm-proj-proj-7".to_string(),
                    window_name: "server".to_string(),
                    dir: "/home/u/Projects/axiom".to_string(),
                    env: vec![],
                    command: "make server".to_string(),
                },
                TmuxCall::SelectWindow {
                    name: "tm-proj-proj-7".to_string(),
                    window: "code".to_string(),
                },
            ]
        );
    }

    #[test]
    fn fresh_session_with_plain_first_window_uses_new_session() {
        let tmux = FakeTmuxOps::new();
        let cfg = ManualConfig {
            dir: Some("/work/dir".to_string()),
            windows: vec![window_entry("fish", None)],
        };
        ensure_manual_session(&tmux, &cfg, Path::new("/home/u"), &identity(), "PROJ-7")
            .expect("should launch");
        assert_eq!(
            tmux.calls(),
            vec![
                TmuxCall::ListWindows,
                TmuxCall::NewSession {
                    name: "tm-proj-proj-7".to_string(),
                    dir: "/work/dir".to_string(),
                    primary_window: "fish".to_string(),
                },
                TmuxCall::SelectWindow {
                    name: "tm-proj-proj-7".to_string(),
                    window: "fish".to_string(),
                },
            ]
        );
    }

    #[test]
    fn existing_session_gets_only_the_missing_windows() {
        let tmux = FakeTmuxOps::new().with_list_windows(Ok(vec![
            tmux_window("tm-proj-proj-7", "audit", false),
            tmux_window("tm-proj-proj-7", "fish", false),
        ]));
        let outcome = ensure_manual_session(
            &tmux,
            &configured(),
            Path::new("/home/u"),
            &identity(),
            "PROJ-7",
        )
        .expect("should launch");

        assert_eq!(outcome.created_windows, vec!["code", "server"]);
        assert_eq!(
            tmux.calls(),
            vec![
                TmuxCall::ListWindows,
                TmuxCall::NewWindowWithCommand {
                    name: "tm-proj-proj-7".to_string(),
                    window_name: "code".to_string(),
                    dir: "/home/u/Projects/axiom".to_string(),
                    env: vec![],
                    command: "nvim".to_string(),
                },
                TmuxCall::NewWindowWithCommand {
                    name: "tm-proj-proj-7".to_string(),
                    window_name: "server".to_string(),
                    dir: "/home/u/Projects/axiom".to_string(),
                    env: vec![],
                    command: "make server".to_string(),
                },
            ],
            "an existing session should not be created again nor refocused"
        );
    }

    #[test]
    fn complete_layout_is_a_no_op_re_press() {
        let tmux = FakeTmuxOps::new().with_list_windows(Ok(vec![
            tmux_window("tm-proj-proj-7", "code", false),
            tmux_window("tm-proj-proj-7", "fish", false),
            tmux_window("tm-proj-proj-7", "server", false),
        ]));
        let outcome = ensure_manual_session(
            &tmux,
            &configured(),
            Path::new("/home/u"),
            &identity(),
            "PROJ-7",
        )
        .expect("should succeed");

        assert!(outcome.created_windows.is_empty());
        assert_eq!(
            tmux.calls(),
            vec![TmuxCall::ListWindows],
            "a complete layout should only be inspected, never mutated"
        );
    }

    #[test]
    fn dead_window_is_replaced_under_a_repeat_suffix() {
        let tmux = FakeTmuxOps::new().with_list_windows(Ok(vec![
            tmux_window("tm-proj-proj-7", "code", true),
            tmux_window("tm-proj-proj-7", "fish", false),
            tmux_window("tm-proj-proj-7", "server", false),
        ]));
        let outcome = ensure_manual_session(
            &tmux,
            &configured(),
            Path::new("/home/u"),
            &identity(),
            "PROJ-7",
        )
        .expect("should launch");

        assert_eq!(outcome.created_windows, vec!["code-2"]);
    }

    #[test]
    fn other_sessions_windows_do_not_count() {
        let tmux = FakeTmuxOps::new().with_list_windows(Ok(vec![tmux_window(
            "tm-proj-proj-8",
            "code",
            false,
        )]));
        let cfg = ManualConfig {
            dir: Some("/work/dir".to_string()),
            windows: vec![window_entry("code", Some("nvim"))],
        };
        let outcome =
            ensure_manual_session(&tmux, &cfg, Path::new("/home/u"), &identity(), "PROJ-7")
                .expect("should launch");
        assert_eq!(outcome.created_windows, vec!["code"]);
        assert!(matches!(
            tmux.calls()[1],
            TmuxCall::NewSessionWithCommand { .. }
        ));
    }

    #[test]
    fn blank_command_is_a_plain_shell_window() {
        let tmux = FakeTmuxOps::new().with_list_windows(Ok(vec![tmux_window(
            "tm-proj-proj-7",
            "code",
            false,
        )]));
        let cfg = ManualConfig {
            dir: Some("/work/dir".to_string()),
            windows: vec![window_entry("code", None), window_entry("fish", Some("  "))],
        };
        ensure_manual_session(&tmux, &cfg, Path::new("/home/u"), &identity(), "PROJ-7")
            .expect("should launch");
        assert_eq!(
            tmux.calls(),
            vec![
                TmuxCall::ListWindows,
                TmuxCall::NewWindow {
                    name: "tm-proj-proj-7".to_string(),
                    window_name: "fish".to_string(),
                    dir: "/work/dir".to_string(),
                },
            ]
        );
    }

    #[test]
    fn tilde_dir_expands_against_home() {
        let tmux = FakeTmuxOps::new();
        let cfg = ManualConfig {
            dir: Some("~/somewhere".to_string()),
            windows: vec![window_entry("code", None)],
        };
        ensure_manual_session(
            &tmux,
            &cfg,
            &PathBuf::from("/Users/jowi"),
            &identity(),
            "PROJ-7",
        )
        .expect("should launch");
        assert!(matches!(
            &tmux.calls()[1],
            TmuxCall::NewSession { dir, .. } if dir == "/Users/jowi/somewhere"
        ));
    }
}
