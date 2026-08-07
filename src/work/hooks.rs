//! Hook script deployment for the lane runner, ported from devtools'
//! `~/devtools/work.ml`'s `deploy_tm_hooks`. The six telemetry/policy hook
//! scripts (`hooks/*.sh` in this repo) are embedded into the `tm` binary via
//! [`include_str!`] and written out to a deploy directory on every run —
//! copy-on-every-run is cheap and idempotent, so there is no install-time
//! step to forget. `tskmstr/hooks/` is now the source of truth; there is no
//! more "devtools repo" these scripts live in (see
//! `docs/plans/runner-port.md` §3). The scripts themselves stay bash+jq —
//! only the deployment and settings-generation logic is ported to Rust.
//!
//! The generated `--settings` JSON is built with `serde_json::json!` rather
//! than the OCaml version's hand-built `sprintf` string, which removes a
//! latent escaping/injection risk for free (see plan §3) — but the *shape*
//! (hook events, matchers, command order) is reproduced exactly.

use std::path::Path;

use serde_json::{Value, json};
use thiserror::Error;

/// Errors that can occur while deploying hook scripts.
#[derive(Debug, Error)]
pub enum HooksError {
    /// Creating the deploy directory failed.
    #[error("failed to create hook deploy dir {path}: {source}")]
    CreateDir {
        /// The directory that could not be created.
        path: std::path::PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Writing a hook script file failed.
    #[error("failed to write hook script {path}: {source}")]
    WriteFile {
        /// The file that could not be written.
        path: std::path::PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Setting the executable bit on a hook script failed.
    #[error("failed to set permissions on {path}: {source}")]
    SetPermissions {
        /// The file whose permissions could not be set.
        path: std::path::PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

/// The seven hook scripts, embedded from `hooks/*.sh` at the repo root, in
/// the exact order `work.ml`'s `tm_hook_names` lists them (plus
/// `tm-session-end.sh`, new for session-usage tracking — see
/// `docs/plans/session-usage.md`). Order here doesn't affect the generated
/// settings JSON (each script is referenced by name where it's wired), but
/// is kept identical to the OCaml source for easy comparison.
const HOOK_SOURCES: &[(&str, &str)] = &[
    ("tm-event.sh", include_str!("../../hooks/tm-event.sh")),
    (
        "tm-checklist.sh",
        include_str!("../../hooks/tm-checklist.sh"),
    ),
    ("tm-usage.sh", include_str!("../../hooks/tm-usage.sh")),
    ("tm-tasklist.sh", include_str!("../../hooks/tm-tasklist.sh")),
    (
        "tm-session-end.sh",
        include_str!("../../hooks/tm-session-end.sh"),
    ),
    (
        "guard-delegate.sh",
        include_str!("../../hooks/guard-delegate.sh"),
    ),
    (
        "graphify-nudge.sh",
        include_str!("../../hooks/graphify-nudge.sh"),
    ),
];

/// Returns the embedded `(filename, contents)` pairs for every hook script.
/// Pure function of the compiled-in sources — no filesystem access.
pub fn hook_scripts() -> &'static [(&'static str, &'static str)] {
    HOOK_SOURCES
}

/// Write every embedded hook script into `deploy_dir`, `chmod 0o755` each
/// one, and return the generated `--settings` JSON document wiring them.
/// Overwrites existing files every call — idempotent, matching `work.ml`'s
/// copy-on-every-run behavior. Creates `deploy_dir` if it doesn't exist.
pub fn deploy_hooks(deploy_dir: &Path) -> Result<Value, HooksError> {
    std::fs::create_dir_all(deploy_dir).map_err(|source| HooksError::CreateDir {
        path: deploy_dir.to_path_buf(),
        source,
    })?;

    for (name, contents) in hook_scripts() {
        let dst = deploy_dir.join(name);
        std::fs::write(&dst, contents).map_err(|source| HooksError::WriteFile {
            path: dst.clone(),
            source,
        })?;
        set_executable(&dst)?;
    }

    Ok(settings_json(deploy_dir))
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), HooksError> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).map_err(|source| {
        HooksError::SetPermissions {
            path: path.to_path_buf(),
            source,
        }
    })
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<(), HooksError> {
    Ok(())
}

/// Build the `--settings` JSON document wiring each hook script into its
/// Claude Code hook event/matcher, mirroring `work.ml`'s `deploy_tm_hooks`
/// `settings_json` exactly:
///
/// ```ocaml
/// { "hooks": {
///     "PreToolUse": [
///       { "matcher": "Edit|Write|MultiEdit|NotebookEdit", "hooks": [ guard-delegate.sh ] },
///       { "matcher": "Bash|Grep", "hooks": [ graphify-nudge.sh ] }
///     ],
///     "PostToolUse": [
///       { "matcher": "TodoWrite", "hooks": [ tm-checklist.sh ] },
///       { "matcher": "TaskCreate|TaskUpdate", "hooks": [ tm-tasklist.sh ] },
///       { "matcher": "*", "hooks": [ tm-event.sh ] }
///     ],
///     "Stop": [ { "hooks": [ tm-usage.sh ] } ],
///     "SubagentStop": [ { "hooks": [ tm-usage.sh ] } ],
///     "SessionEnd": [ { "hooks": [ tm-session-end.sh ] } ]
/// } }
/// ```
///
/// `tm-usage.sh` is wired into both `Stop` and `SubagentStop`, exactly as
/// `work.ml` does (it's the only script referenced twice). `SessionEnd` is
/// new for session-usage tracking (`docs/plans/session-usage.md`) — it has
/// no `work.ml` counterpart, since lane runs never fire `SessionEnd`
/// (the wrapper process, not an interactive Claude Code session, owns
/// finish there).
fn settings_json(deploy_dir: &Path) -> Value {
    let cmd = |name: &str| {
        json!({
            "type": "command",
            "command": deploy_dir.join(name).to_string_lossy().into_owned(),
        })
    };

    json!({
        "hooks": {
            "PreToolUse": [
                { "matcher": "Edit|Write|MultiEdit|NotebookEdit", "hooks": [ cmd("guard-delegate.sh") ] },
                { "matcher": "Bash|Grep", "hooks": [ cmd("graphify-nudge.sh") ] }
            ],
            "PostToolUse": [
                { "matcher": "TodoWrite", "hooks": [ cmd("tm-checklist.sh") ] },
                { "matcher": "TaskCreate|TaskUpdate", "hooks": [ cmd("tm-tasklist.sh") ] },
                { "matcher": "*", "hooks": [ cmd("tm-event.sh") ] }
            ],
            "Stop": [
                { "hooks": [ cmd("tm-usage.sh") ] }
            ],
            "SubagentStop": [
                { "hooks": [ cmd("tm-usage.sh") ] }
            ],
            "SessionEnd": [
                { "hooks": [ cmd("tm-session-end.sh") ] }
            ]
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_hook_names() -> Vec<&'static str> {
        vec![
            "tm-event.sh",
            "tm-checklist.sh",
            "tm-usage.sh",
            "tm-tasklist.sh",
            "tm-session-end.sh",
            "guard-delegate.sh",
            "graphify-nudge.sh",
        ]
    }

    #[test]
    fn hook_scripts_returns_all_seven_by_name() {
        let names: Vec<&str> = hook_scripts().iter().map(|(n, _)| *n).collect();
        for expected in all_hook_names() {
            assert!(names.contains(&expected), "missing hook script {expected}");
        }
        assert_eq!(hook_scripts().len(), 7);
    }

    #[test]
    fn hook_scripts_contents_match_tracked_files_byte_for_byte() {
        for (name, contents) in hook_scripts() {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("hooks")
                .join(name);
            let on_disk = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
            assert_eq!(
                *contents, on_disk,
                "embedded contents for {name} differ from hooks/{name} on disk"
            );
        }
    }

    #[test]
    fn deploy_hooks_writes_every_script_byte_identical_to_embedded_source() {
        let dir = tempfile::tempdir().expect("tempdir");
        deploy_hooks(dir.path()).expect("deploy_hooks");

        for (name, contents) in hook_scripts() {
            let written = std::fs::read_to_string(dir.path().join(name))
                .unwrap_or_else(|e| panic!("failed to read deployed {name}: {e}"));
            assert_eq!(
                &written, contents,
                "deployed {name} differs from embedded source"
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn deploy_hooks_sets_executable_bit_on_every_script() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        deploy_hooks(dir.path()).expect("deploy_hooks");

        for (name, _) in hook_scripts() {
            let meta = std::fs::metadata(dir.path().join(name)).expect("metadata");
            let mode = meta.permissions().mode() & 0o777;
            assert_eq!(mode, 0o755, "{name} has mode {mode:o}, expected 0o755");
        }
    }

    #[test]
    fn deploy_hooks_is_idempotent_overwriting_on_every_call() {
        let dir = tempfile::tempdir().expect("tempdir");
        deploy_hooks(dir.path()).expect("first deploy_hooks");
        // Simulate drift: hand-edit a deployed file, then redeploy.
        let target = dir.path().join("tm-event.sh");
        std::fs::write(&target, "tampered").expect("tamper");
        deploy_hooks(dir.path()).expect("second deploy_hooks");

        let restored = std::fs::read_to_string(&target).expect("read restored");
        let (_, expected) = hook_scripts()
            .iter()
            .find(|(n, _)| *n == "tm-event.sh")
            .unwrap();
        assert_eq!(&restored, expected);
    }

    #[test]
    fn deploy_hooks_creates_deploy_dir_if_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("does/not/exist/yet");
        assert!(!nested.exists());

        deploy_hooks(&nested).expect("deploy_hooks should create the dir");

        assert!(nested.join("tm-event.sh").exists());
    }

    #[test]
    fn settings_json_has_expected_hook_event_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        let settings = deploy_hooks(dir.path()).expect("deploy_hooks");

        let hooks = settings
            .get("hooks")
            .expect("hooks key")
            .as_object()
            .expect("object");
        for expected_event in [
            "PreToolUse",
            "PostToolUse",
            "Stop",
            "SubagentStop",
            "SessionEnd",
        ] {
            assert!(
                hooks.contains_key(expected_event),
                "missing hook event {expected_event}"
            );
        }
    }

    #[test]
    fn settings_json_pre_tool_use_matchers_and_commands_match_work_ml() {
        let dir = tempfile::tempdir().expect("tempdir");
        let settings = deploy_hooks(dir.path()).expect("deploy_hooks");

        let pre = settings["hooks"]["PreToolUse"].as_array().expect("array");
        assert_eq!(pre.len(), 2);

        assert_eq!(pre[0]["matcher"], "Edit|Write|MultiEdit|NotebookEdit");
        assert_eq!(pre[0]["hooks"][0]["type"], "command");
        assert_eq!(
            pre[0]["hooks"][0]["command"],
            dir.path()
                .join("guard-delegate.sh")
                .to_string_lossy()
                .into_owned()
        );

        assert_eq!(pre[1]["matcher"], "Bash|Grep");
        assert_eq!(
            pre[1]["hooks"][0]["command"],
            dir.path()
                .join("graphify-nudge.sh")
                .to_string_lossy()
                .into_owned()
        );
    }

    #[test]
    fn settings_json_post_tool_use_matchers_and_commands_match_work_ml() {
        let dir = tempfile::tempdir().expect("tempdir");
        let settings = deploy_hooks(dir.path()).expect("deploy_hooks");

        let post = settings["hooks"]["PostToolUse"].as_array().expect("array");
        assert_eq!(post.len(), 3);

        assert_eq!(post[0]["matcher"], "TodoWrite");
        assert_eq!(
            post[0]["hooks"][0]["command"],
            dir.path()
                .join("tm-checklist.sh")
                .to_string_lossy()
                .into_owned()
        );

        assert_eq!(post[1]["matcher"], "TaskCreate|TaskUpdate");
        assert_eq!(
            post[1]["hooks"][0]["command"],
            dir.path()
                .join("tm-tasklist.sh")
                .to_string_lossy()
                .into_owned()
        );

        assert_eq!(post[2]["matcher"], "*");
        assert_eq!(
            post[2]["hooks"][0]["command"],
            dir.path()
                .join("tm-event.sh")
                .to_string_lossy()
                .into_owned()
        );
    }

    #[test]
    fn settings_json_stop_and_subagent_stop_both_wire_tm_usage_sh() {
        let dir = tempfile::tempdir().expect("tempdir");
        let settings = deploy_hooks(dir.path()).expect("deploy_hooks");

        let usage_path = dir
            .path()
            .join("tm-usage.sh")
            .to_string_lossy()
            .into_owned();

        let stop = settings["hooks"]["Stop"].as_array().expect("array");
        assert_eq!(stop.len(), 1);
        assert!(stop[0]["hooks"].get(0).is_some());
        assert_eq!(stop[0]["hooks"][0]["command"], usage_path.clone());

        let subagent_stop = settings["hooks"]["SubagentStop"].as_array().expect("array");
        assert_eq!(subagent_stop.len(), 1);
        assert_eq!(subagent_stop[0]["hooks"][0]["command"], usage_path);
    }

    #[test]
    fn settings_json_session_end_wires_tm_session_end_sh() {
        let dir = tempfile::tempdir().expect("tempdir");
        let settings = deploy_hooks(dir.path()).expect("deploy_hooks");

        let session_end = settings["hooks"]["SessionEnd"].as_array().expect("array");
        assert_eq!(session_end.len(), 1);
        assert_eq!(
            session_end[0]["hooks"][0]["command"],
            dir.path()
                .join("tm-session-end.sh")
                .to_string_lossy()
                .into_owned()
        );
    }

    #[test]
    fn settings_json_round_trips_through_serde_json_string() {
        let dir = tempfile::tempdir().expect("tempdir");
        let settings = deploy_hooks(dir.path()).expect("deploy_hooks");

        let s = serde_json::to_string(&settings).expect("serialize");
        let parsed: Value = serde_json::from_str(&s).expect("parse back");
        assert_eq!(parsed, settings);
    }

    // --- Parity checklist (docs/plans/runner-port.md §7 "Undocumented
    // tolerance behaviors") — these assert the risky fallback logic is
    // still textually present in the tracked scripts, since the scripts
    // themselves are copied verbatim rather than re-tested behaviorally
    // here (that would mean re-implementing bash+jq in Rust).

    #[test]
    fn parity_tm_tasklist_keeps_task_hash_n_text_fallback() {
        let (_, contents) = hook_scripts()
            .iter()
            .find(|(n, _)| *n == "tm-tasklist.sh")
            .unwrap();
        assert!(
            contents.contains("grep -oE 'Task #[0-9]+'"),
            "tm-tasklist.sh lost its \"Task #N\" text-parsing fallback for older Claude Code versions"
        );
        assert!(contents.contains(".tool_response.task.id"));
    }

    #[test]
    fn parity_graphify_nudge_keeps_five_per_session_rate_limit() {
        let (_, contents) = hook_scripts()
            .iter()
            .find(|(n, _)| *n == "graphify-nudge.sh")
            .unwrap();
        assert!(
            contents.contains("-ge 5"),
            "graphify-nudge.sh lost its 5-per-session rate limit"
        );
        assert!(contents.contains("COUNT_FILE"));
    }

    #[test]
    fn parity_guard_delegate_keeps_deny_logic_and_subagent_bypass() {
        let (_, contents) = hook_scripts()
            .iter()
            .find(|(n, _)| *n == "guard-delegate.sh")
            .unwrap();
        assert!(
            contents.contains("permissionDecision: \"deny\""),
            "guard-delegate.sh lost its deny decision"
        );
        assert!(
            contents.contains("AGENT_ID") && contents.contains("exit 0"),
            "guard-delegate.sh lost its subagent bypass (agent_id present -> allow)"
        );
    }

    // --- Session-usage gating parity (docs/plans/session-usage.md "Hook
    // gating: marker fallback") — the four telemetry scripts must keep the
    // marker-fallback path so registered interactive sessions (audit/create)
    // get telemetry, while guard-delegate.sh must NOT gain it (that would
    // start denying edits in registered interactive sessions, not just lane
    // runs).

    #[test]
    fn parity_telemetry_scripts_keep_session_marker_fallback() {
        for name in [
            "tm-event.sh",
            "tm-usage.sh",
            "tm-checklist.sh",
            "tm-tasklist.sh",
        ] {
            let (_, contents) = hook_scripts().iter().find(|(n, _)| *n == name).unwrap();
            assert!(
                contents.contains("tskmstr/sessions"),
                "{name} lost its session-marker fallback path"
            );
        }
    }

    #[test]
    fn parity_guard_delegate_excludes_session_marker_fallback() {
        let (_, contents) = hook_scripts()
            .iter()
            .find(|(n, _)| *n == "guard-delegate.sh")
            .unwrap();
        assert!(
            !contents.contains("tskmstr/sessions"),
            "guard-delegate.sh must not gain the session-marker fallback — it would start denying edits in registered interactive sessions"
        );
    }

    #[test]
    fn parity_tm_session_end_finishes_run_and_removes_marker() {
        let (_, contents) = hook_scripts()
            .iter()
            .find(|(n, _)| *n == "tm-session-end.sh")
            .unwrap();
        assert!(
            contents.contains("--status done"),
            "tm-session-end.sh lost its finish-with-done-status call"
        );
        assert!(
            contents.contains("rm -f"),
            "tm-session-end.sh lost its marker cleanup"
        );
        // Gate polarity matters here, opposite to every other telemetry
        // hook: this hook must exit when TSKMSTR_RUN_ID IS set (the lane
        // wrapper owns finish) and proceed when it is not.
        assert!(
            contents.contains(r#"[ -n "${TSKMSTR_RUN_ID:-}" ] && exit 0"#),
            "tm-session-end.sh lost its inverted lane-run gate"
        );
        assert!(
            !contents.contains(r#"[ -z "${TSKMSTR_RUN_ID:-}" ] && exit 0"#),
            "tm-session-end.sh must not exit on unset TSKMSTR_RUN_ID — that would skip every interactive session"
        );
    }
}
