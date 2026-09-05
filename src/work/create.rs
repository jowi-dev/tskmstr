//! Board-launched ticket-*creation* sessions (GitHub issue #15): the `c`
//! key's launch half. Like an audit (see [`crate::work::audit`]), a create
//! session is an interactive `claude` conversation and so is tmux-hosted —
//! but three things set it apart:
//!
//! - **Keyless.** No ticket exists when the session launches, so per-ticket
//!   session naming cannot apply; the whole scope shares one
//!   [`crate::work::naming::create_session_name`] session, and a second
//!   launch while it is live is an [`CreateLaunchError::AlreadyRunning`] the
//!   board answers by attaching rather than duplicating.
//! - **No run pre-registration.** An audit pre-registers a run row for the
//!   in-session `tm ticket audit` to adopt; here there is no ticket to hang
//!   a row on. `tm ticket create` already registers a `kind = "create"` run
//!   through the session marker once the ticket exists (see
//!   `crate::runs::session::register_session`), so launch-side telemetry
//!   plumbing would be redundant.
//! - **Attached immediately.** The point of the flow is to start dictating a
//!   ticket right away, so the board attaches the moment the launch returns
//!   (that attach is the board's job, not this module's — same split as
//!   audit's launch/attach).

use std::path::Path;

use thiserror::Error;

use crate::config::{BackendIdentity, CreateConfig};
use crate::work::naming::{create_session_name, expand_tilde};
use crate::work::tmux::{
    TmuxError, TmuxOps, has_live_window, session_window_names, unique_window_name,
};

/// Default prompt used when [`CreateConfig::prompt`] is unset. Unlike
/// [`crate::work::audit`]'s template, no `{key}` substitution applies —
/// there is no ticket key yet.
const DEFAULT_PROMPT: &str = "/ticket-create";

/// Name of the tmux window a launched create session's `claude` process runs
/// in.
pub const CREATE_WINDOW_NAME: &str = "create";

/// Errors returned by [`launch_create`].
#[derive(Debug, Error)]
pub enum CreateLaunchError {
    /// `[work.create].dir` is unset, so there is nowhere to launch the
    /// session. A status-line error for the caller to surface, not a crash —
    /// same posture as [`crate::work::audit::AuditLaunchError::NotConfigured`].
    #[error("ticket creation is not configured; set [work.create].dir")]
    NotConfigured,

    /// The scope's create window is already live. The caller should attach
    /// to `session_name` instead of launching a second draft — issue #15's
    /// "deliberate re-entry" criterion.
    #[error("a create session is already running: {session_name}:{window_name}")]
    AlreadyRunning {
        /// Name of the tmux session holding the live window.
        session_name: String,
        /// Name of the already-live window.
        window_name: String,
    },

    /// Shelling out to `tmux` failed.
    #[error(transparent)]
    Tmux(#[from] TmuxError),
}

/// Successful outcome of [`launch_create`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateLaunchOutcome {
    /// Name of the tmux session the caller should attach to.
    pub session_name: String,
    /// Name of the window `claude` runs in: [`CREATE_WINDOW_NAME`], or a
    /// [`unique_window_name`] suffixed variant when a previous create
    /// session's dead window still holds that name.
    pub window_name: String,
}

/// The prompt text handed to `claude` on launch: `template` verbatim, or
/// [`DEFAULT_PROMPT`] when `template` is `None`.
pub fn create_prompt(template: Option<&str>) -> String {
    template.unwrap_or(DEFAULT_PROMPT).to_string()
}

/// Launches (or refuses to double-launch) the scope's ticket-creation
/// session.
///
/// 1. Errors with [`CreateLaunchError::NotConfigured`] if `create_cfg.dir`
///    is unset.
/// 2. Errors with [`CreateLaunchError::AlreadyRunning`] if a live window
///    named [`CREATE_WINDOW_NAME`] already exists in the scope's
///    [`create_session_name`] session — the caller attaches to it instead.
/// 3. Otherwise starts `claude <prompt>` (with `--model` when
///    `create_cfg.model` is set — see
///    [`crate::work::audit::claude_command`]) in that window: the session is
///    created if this is the scope's first create launch, and the window
///    appended to it otherwise, taking a [`unique_window_name`] suffix if a
///    dead predecessor still holds the plain name. An appended window is
///    also selected, so the immediate attach lands on the fresh `claude`
///    rather than the dead aftermath.
///
/// No run row is created and no environment is injected — see the module
/// docs' "No run pre-registration".
///
/// `home` resolves a leading `~` in `create_cfg.dir` via [`expand_tilde`],
/// and `identity`'s [`session_slug`](BackendIdentity::session_slug)
/// qualifies the session name, both exactly as
/// [`crate::work::audit::launch_audit`] does.
pub fn launch_create(
    tmux: &dyn TmuxOps,
    create_cfg: &CreateConfig,
    home: &Path,
    identity: &BackendIdentity,
) -> Result<CreateLaunchOutcome, CreateLaunchError> {
    let raw_dir = create_cfg
        .dir
        .as_deref()
        .ok_or(CreateLaunchError::NotConfigured)?;
    let dir = expand_tilde(raw_dir, home);
    let dir_str = dir.to_string_lossy().into_owned();

    let session_name = create_session_name(&identity.session_slug());
    // One `list_windows` snapshot answers both "is a create session already
    // running?" and "does the session exist yet?", mirroring `launch_audit`.
    let windows = tmux.list_windows()?;
    if has_live_window(&windows, &session_name, CREATE_WINDOW_NAME) {
        return Err(CreateLaunchError::AlreadyRunning {
            session_name,
            window_name: CREATE_WINDOW_NAME.to_string(),
        });
    }
    let existing_windows = session_window_names(&windows, &session_name);
    let window_name = unique_window_name(CREATE_WINDOW_NAME, &existing_windows);

    let prompt = create_prompt(create_cfg.prompt.as_deref());
    let command = crate::work::audit::claude_command(create_cfg.model.as_deref(), &prompt);

    if existing_windows.is_empty() {
        tmux.new_session_with_command(&session_name, &dir_str, &window_name, &[], &command)?;
    } else {
        tmux.new_window_with_command(&session_name, &window_name, &dir_str, &[], &command)?;
        tmux.select_window(&session_name, &window_name)?;
    }

    Ok(CreateLaunchOutcome {
        session_name,
        window_name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::tmux::{FakeTmuxOps, TmuxCall, TmuxWindow};
    use std::path::PathBuf;

    fn configured(dir: &str) -> CreateConfig {
        CreateConfig {
            dir: Some(dir.to_string()),
            prompt: None,
            model: None,
        }
    }

    /// Canonical test identity; its `session_slug()` is `proj`, so the
    /// create session in these tests is named `tm-proj-create`.
    fn test_identity() -> BackendIdentity {
        BackendIdentity::Jira {
            base_url: "https://x.atlassian.net".to_string(),
            project_key: "PROJ".to_string(),
        }
    }

    /// A [`FakeTmuxOps`] whose `list_windows` snapshot is `windows`, each
    /// entry given as `(session, window, dead)`.
    fn tmux_with_windows(windows: &[(&str, &str, bool)]) -> FakeTmuxOps {
        FakeTmuxOps::new().with_list_windows(Ok(windows
            .iter()
            .map(|(session, name, dead)| TmuxWindow {
                session: (*session).to_string(),
                name: (*name).to_string(),
                dead: *dead,
            })
            .collect()))
    }

    #[test]
    fn create_prompt_defaults_to_ticket_create() {
        assert_eq!(create_prompt(None), "/ticket-create");
    }

    #[test]
    fn create_prompt_uses_custom_template_verbatim() {
        assert_eq!(create_prompt(Some("/my-create")), "/my-create");
    }

    #[test]
    fn launch_create_starts_a_keyless_session_with_no_env() {
        let tmux = FakeTmuxOps::new();
        let home = PathBuf::from("/Users/jowi");
        let cfg = configured("~/Projects/axiom");

        let outcome =
            launch_create(&tmux, &cfg, &home, &test_identity()).expect("launch should succeed");

        assert_eq!(outcome.session_name, "tm-proj-create");
        assert_eq!(outcome.window_name, "create");
        assert_eq!(
            tmux.calls(),
            vec![
                TmuxCall::ListWindows,
                TmuxCall::NewSessionWithCommand {
                    name: "tm-proj-create".to_string(),
                    dir: "/Users/jowi/Projects/axiom".to_string(),
                    window_name: "create".to_string(),
                    env: vec![],
                    command: "claude '/ticket-create'".to_string(),
                },
            ]
        );
    }

    #[test]
    fn launch_create_errors_when_dir_is_unset() {
        let tmux = FakeTmuxOps::new();
        let home = PathBuf::from("/Users/jowi");
        let cfg = CreateConfig::default();

        let err = launch_create(&tmux, &cfg, &home, &test_identity())
            .expect_err("should refuse to launch");

        assert!(matches!(err, CreateLaunchError::NotConfigured));
        assert!(tmux.calls().is_empty());
    }

    #[test]
    fn launch_create_errors_when_the_create_window_is_live() {
        let tmux = tmux_with_windows(&[("tm-proj-create", "create", false)]);
        let home = PathBuf::from("/Users/jowi");
        let cfg = configured("/repo/axiom");

        let err = launch_create(&tmux, &cfg, &home, &test_identity())
            .expect_err("should refuse to double-launch");

        match err {
            CreateLaunchError::AlreadyRunning {
                session_name,
                window_name,
            } => {
                assert_eq!(session_name, "tm-proj-create");
                assert_eq!(window_name, "create");
            }
            other => panic!("expected AlreadyRunning, got {other:?}"),
        }
    }

    #[test]
    fn launch_create_relaunches_over_a_dead_window_with_a_suffix_and_selects_it() {
        // A window whose pane exited is aftermath, not a running draft: the
        // relaunch appends a suffixed window and selects it so the immediate
        // attach lands on the fresh claude.
        let tmux = tmux_with_windows(&[("tm-proj-create", "create", true)]);
        let home = PathBuf::from("/Users/jowi");
        let cfg = configured("/repo/axiom");

        let outcome =
            launch_create(&tmux, &cfg, &home, &test_identity()).expect("launch should succeed");

        assert_eq!(outcome.window_name, "create-2");
        assert_eq!(
            tmux.calls(),
            vec![
                TmuxCall::ListWindows,
                TmuxCall::NewWindowWithCommand {
                    name: "tm-proj-create".to_string(),
                    window_name: "create-2".to_string(),
                    dir: "/repo/axiom".to_string(),
                    env: vec![],
                    command: "claude '/ticket-create'".to_string(),
                },
                TmuxCall::SelectWindow {
                    name: "tm-proj-create".to_string(),
                    window: "create-2".to_string(),
                },
            ]
        );
    }

    #[test]
    fn launch_create_ignores_other_scopes_create_sessions() {
        // Another repo's create session must not read as "already running"
        // here — the scope slug is what keeps them apart (issue #10's rule).
        let tmux = tmux_with_windows(&[("tm-other-create", "create", false)]);
        let home = PathBuf::from("/Users/jowi");
        let cfg = configured("/repo/axiom");

        let outcome =
            launch_create(&tmux, &cfg, &home, &test_identity()).expect("launch should succeed");

        assert_eq!(outcome.session_name, "tm-proj-create");
    }

    #[test]
    fn launch_create_uses_custom_prompt_and_model() {
        let tmux = FakeTmuxOps::new();
        let home = PathBuf::from("/Users/jowi");
        let cfg = CreateConfig {
            dir: Some("/repo/axiom".to_string()),
            prompt: Some("/my-create".to_string()),
            model: Some("opus".to_string()),
        };

        launch_create(&tmux, &cfg, &home, &test_identity()).unwrap();

        let command = tmux.calls().iter().find_map(|call| match call {
            TmuxCall::NewSessionWithCommand { command, .. } => Some(command.clone()),
            _ => None,
        });
        assert_eq!(
            command,
            Some("claude --model 'opus' '/my-create'".to_string())
        );
    }
}
