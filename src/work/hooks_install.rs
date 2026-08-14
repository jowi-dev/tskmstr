//! `tm work hooks install --user`: install tm's telemetry hooks into a
//! user's *interactive* Claude Code settings, not a lane worktree.
//!
//! [`crate::work::hooks`] deploys hook scripts + a generated `settings.json`
//! into a lane worktree on every `tm work run` — copy-on-every-run, no
//! install step. Interactive sessions (`tm ticket audit`, `tm ticket
//! create`) never go through that path: they run in the user's normal
//! Claude Code, which reads `~/.claude/settings.json` (or
//! `$CLAUDE_CONFIG_DIR/settings.json`), and nothing has ever installed tm's
//! hooks there. This module closes that gap with a one-time (idempotent,
//! re-runnable) install.
//!
//! # Scope: three entries only
//!
//! Unlike the lane path, which wires every hook script into every event,
//! this only ever adds:
//!
//! - `Stop` -> `tm-usage.sh`
//! - `SubagentStop` -> `tm-usage.sh`
//! - `SessionEnd` -> `tm-session-end.sh`
//!
//! `guard-delegate.sh` is deliberately never wired in here: it `deny`s
//! main-loop edits while its gate is active, and installing it user-wide
//! would start blocking the user's ordinary editing in every session, not
//! just lane runs. `tm-event.sh`/`tm-checklist.sh`/`tm-tasklist.sh`/
//! `tm-session-state.sh` are excluded too — they add per-tool-call overhead
//! to every interactive session and aren't needed to close the cost gap
//! this module exists for (see `docs/plans/session-usage.md`). See
//! [`USER_HOOK_WIRING`] and the `never_wires_guard_delegate_at_user_level`
//! test.
//!
//! # Safety posture
//!
//! `~/.claude/settings.json` is shared with unrelated tools (other hook
//! installers, editor integrations, etc.), so [`install_user_hooks`] is:
//!
//! - **Purely additive**: existing hook entries are never removed,
//!   reordered, or rewritten; only new entries are appended, and only when
//!   an equivalent one (matched by command path) isn't already present.
//! - **Idempotent**: running it twice never duplicates an entry.
//! - **Backed up**: a byte-for-byte copy of the settings file is written
//!   next to it (`settings.json.bak-<suffix>`) before every non-dry-run
//!   write, using a caller-supplied `backup_suffix` rather than reading the
//!   clock in here — see that parameter's doc comment.
//! - **Safe on malformed input**: an absent, empty, or unparseable settings
//!   file is a hard [`HooksInstallError`], never silently overwritten or
//!   recreated from scratch.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use thiserror::Error;

use crate::work::hooks::{HooksError, deploy_hooks, hook_scripts};

/// The event -> script -> timeout wiring this module installs at user
/// level. Deliberately a small, hand-picked subset of
/// [`crate::work::hooks::hook_scripts`] — see the module docs for why
/// `guard-delegate.sh` and the four other lane-only scripts are excluded.
pub const USER_HOOK_WIRING: &[(&str, &str, u64)] = &[
    ("Stop", "tm-usage.sh", 30),
    ("SubagentStop", "tm-usage.sh", 30),
    ("SessionEnd", "tm-session-end.sh", 30),
];

/// Errors surfaced by [`install_user_hooks`].
#[derive(Debug, Error)]
pub enum HooksInstallError {
    /// Copying hook scripts into the XDG hooks directory failed.
    #[error(transparent)]
    Hooks(#[from] HooksError),

    /// The settings file doesn't exist. Deliberately not auto-created —
    /// see the module docs' "safe on malformed input" bullet.
    #[error(
        "{path} does not exist; run Claude Code at least once to create it, then re-run `tm work hooks install --user`",
        path = .path.display()
    )]
    MissingSettings {
        /// The settings file that was expected to exist.
        path: PathBuf,
    },

    /// The settings file exists but couldn't be read.
    #[error("failed to read {path}: {source}", path = .path.display())]
    ReadSettings {
        /// The settings file that couldn't be read.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: io::Error,
    },

    /// The settings file exists but is empty.
    #[error(
        "{path} is empty; refusing to treat that as valid JSON — inspect/restore it by hand first",
        path = .path.display()
    )]
    EmptySettings {
        /// The empty settings file.
        path: PathBuf,
    },

    /// The settings file exists but isn't valid JSON.
    #[error(
        "{path} is not valid JSON ({source}); refusing to modify a file we can't safely parse — inspect/restore it by hand first",
        path = .path.display()
    )]
    InvalidSettingsJson {
        /// The unparseable settings file.
        path: PathBuf,
        /// The underlying parse error.
        #[source]
        source: serde_json::Error,
    },

    /// The settings file parses, but its top-level value isn't a JSON
    /// object (e.g. an array or a bare string).
    #[error(
        "{path}'s top-level JSON is not an object; refusing to modify it",
        path = .path.display()
    )]
    NotAnObject {
        /// The settings file whose shape is unexpected.
        path: PathBuf,
    },

    /// A `hooks.<event>` value exists but isn't a JSON array.
    #[error(
        "{path}'s hooks.{event} is not an array; refusing to modify it",
        path = .path.display()
    )]
    EventNotArray {
        /// The settings file whose shape is unexpected.
        path: PathBuf,
        /// The hook event key whose value has the wrong shape.
        event: String,
    },

    /// The `hooks` key exists but isn't a JSON object.
    #[error(
        "{path}'s top-level \"hooks\" value is not an object; refusing to modify it",
        path = .path.display()
    )]
    HooksNotObject {
        /// The settings file whose shape is unexpected.
        path: PathBuf,
    },

    /// Writing the pre-modification backup failed.
    #[error("failed to write backup {path}: {source}", path = .path.display())]
    BackupWrite {
        /// The backup file that couldn't be written.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: io::Error,
    },

    /// Writing the merged settings file failed.
    #[error("failed to write {path}: {source}", path = .path.display())]
    WriteSettings {
        /// The settings file that couldn't be written.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: io::Error,
    },
}

/// Resolves the stable XDG hooks directory tm copies hook scripts into for
/// user-level installs: `${XDG_DATA_HOME:-<home>/.local/share}/tskmstr/hooks`.
/// Mirrors `crate::runs`' run-database path resolution exactly (same env
/// var, same fallback), just naming a directory instead of a file. Pure
/// function of its inputs — callers resolve `$XDG_DATA_HOME` themselves so
/// this stays testable without touching the real environment.
pub fn user_hooks_dir(xdg_data_home: Option<&Path>, home: &Path) -> PathBuf {
    let data_home = xdg_data_home
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".local/share"));
    data_home.join("tskmstr").join("hooks")
}

/// Resolves Claude Code's own settings file: `$CLAUDE_CONFIG_DIR/settings.json`
/// when set, else `<home>/.claude/settings.json`. Pure function of its
/// inputs, for the same testability reason as [`user_hooks_dir`].
pub fn user_settings_path(claude_config_dir: Option<&Path>, home: &Path) -> PathBuf {
    let config_dir = claude_config_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".claude"));
    config_dir.join("settings.json")
}

/// The outcome of an [`install_user_hooks`] call — everything
/// `tm work hooks install --user` needs to print its summary.
#[derive(Debug, Clone, Default)]
pub struct InstallReport {
    /// Hook script filenames written (missing, or present but stale).
    pub scripts_copied: Vec<String>,
    /// Hook script filenames already byte-identical on disk.
    pub scripts_already_present: Vec<String>,
    /// `"<event> -> <script>"` labels newly appended to `settings.json`.
    pub hooks_added: Vec<String>,
    /// `"<event> -> <script>"` labels already present in `settings.json`.
    pub hooks_already_present: Vec<String>,
    /// Where the pre-modification backup was written, if this wasn't a
    /// dry run.
    pub backup_path: Option<PathBuf>,
    /// Whether this call was a dry run (nothing was written).
    pub dry_run: bool,
}

impl InstallReport {
    /// Render a plain-text (no emoji) summary of what changed/would
    /// change, matching the rest of the CLI's output style.
    pub fn write_summary(&self, out: &mut dyn Write) -> io::Result<()> {
        if self.dry_run {
            writeln!(out, "Dry run: no changes written.")?;
        }

        writeln!(out, "Hook scripts:")?;
        if self.scripts_copied.is_empty() {
            writeln!(out, "  copied: (none)")?;
        } else {
            writeln!(out, "  copied: {}", self.scripts_copied.join(", "))?;
        }
        writeln!(
            out,
            "  already up to date: {}",
            if self.scripts_already_present.is_empty() {
                "(none)".to_string()
            } else {
                self.scripts_already_present.join(", ")
            }
        )?;

        writeln!(out, "settings.json hook wiring:")?;
        writeln!(
            out,
            "  added: {}",
            if self.hooks_added.is_empty() {
                "(none)".to_string()
            } else {
                self.hooks_added.join(", ")
            }
        )?;
        writeln!(
            out,
            "  already present: {}",
            if self.hooks_already_present.is_empty() {
                "(none)".to_string()
            } else {
                self.hooks_already_present.join(", ")
            }
        )?;

        match &self.backup_path {
            Some(path) => writeln!(out, "Backup written to: {}", path.display())?,
            None if !self.dry_run => writeln!(out, "Backup written to: (none)")?,
            None => {}
        }

        Ok(())
    }
}

/// Which of [`hook_scripts`]' embedded scripts are missing or stale (not
/// byte-identical) under `xdg_hooks_dir`, without writing anything. Used
/// both for the dry-run report and to build the real report's "copied"
/// list before [`deploy_hooks`] actually writes.
fn scripts_status(xdg_hooks_dir: &Path) -> (Vec<String>, Vec<String>) {
    let mut stale = Vec::new();
    let mut up_to_date = Vec::new();

    for (name, contents) in hook_scripts() {
        let path = xdg_hooks_dir.join(name);
        let matches = std::fs::read(&path)
            .map(|on_disk| on_disk == contents.as_bytes())
            .unwrap_or(false);
        if matches {
            up_to_date.push((*name).to_string());
        } else {
            stale.push((*name).to_string());
        }
    }

    (stale, up_to_date)
}

/// Appends any missing [`USER_HOOK_WIRING`] entries into `settings`'
/// `hooks.<event>` arrays, creating `hooks` and any missing event array as
/// needed. Never touches an existing hook-group entry — only ever pushes a
/// brand-new one. Returns `(added, already_present)` labels.
///
/// Detects an already-installed entry by comparing the full resolved
/// command path (`xdg_hooks_dir/<script>`), which is exactly what this
/// module writes both now and on a prior run — deterministic per machine,
/// so exact string equality is enough for idempotency.
fn merge_user_hooks(
    settings: &mut Value,
    xdg_hooks_dir: &Path,
    settings_path: &Path,
) -> Result<(Vec<String>, Vec<String>), HooksInstallError> {
    let mut added = Vec::new();
    let mut already_present = Vec::new();

    let top = settings
        .as_object_mut()
        .ok_or_else(|| HooksInstallError::NotAnObject {
            path: settings_path.to_path_buf(),
        })?;

    let hooks_value = top.entry("hooks").or_insert_with(|| json!({}));
    let hooks_obj =
        hooks_value
            .as_object_mut()
            .ok_or_else(|| HooksInstallError::HooksNotObject {
                path: settings_path.to_path_buf(),
            })?;

    for (event, script, timeout) in USER_HOOK_WIRING {
        let command_path = xdg_hooks_dir.join(script).to_string_lossy().into_owned();
        let label = format!("{event} -> {script}");

        let event_value = hooks_obj
            .entry(event.to_string())
            .or_insert_with(|| json!([]));
        let event_array =
            event_value
                .as_array_mut()
                .ok_or_else(|| HooksInstallError::EventNotArray {
                    path: settings_path.to_path_buf(),
                    event: event.to_string(),
                })?;

        let already = event_array.iter().any(|group| {
            group
                .get("hooks")
                .and_then(Value::as_array)
                .map(|hs| {
                    hs.iter().any(|h| {
                        h.get("command").and_then(Value::as_str) == Some(command_path.as_str())
                    })
                })
                .unwrap_or(false)
        });

        if already {
            already_present.push(label);
        } else {
            event_array.push(json!({
                "hooks": [
                    { "type": "command", "command": command_path, "timeout": timeout }
                ]
            }));
            added.push(label);
        }
    }

    Ok((added, already_present))
}

fn backup_path_for(settings_path: &Path, backup_suffix: &str) -> PathBuf {
    let file_name = settings_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "settings.json".to_string());
    settings_path.with_file_name(format!("{file_name}.bak-{backup_suffix}"))
}

/// Install tm's user-level telemetry hooks (see the module docs) into
/// `settings_path`, copying hook scripts into `xdg_hooks_dir` first.
///
/// `backup_suffix` names the pre-modification backup file
/// (`settings.json.bak-<backup_suffix>`); callers supply it rather than
/// this function reading the clock, so tests stay deterministic — real
/// callers derive it from `crate::work::run::Clock`/`naming::format_timestamp`,
/// the same pattern already used for lane branch names.
///
/// `dry_run = true` performs every read and every idempotency check but
/// writes nothing: no hook scripts, no backup, no settings file. The
/// returned [`InstallReport`] describes exactly what a non-dry-run call
/// would do.
pub fn install_user_hooks(
    xdg_hooks_dir: &Path,
    settings_path: &Path,
    backup_suffix: &str,
    dry_run: bool,
) -> Result<InstallReport, HooksInstallError> {
    let (scripts_copied, scripts_already_present) = scripts_status(xdg_hooks_dir);

    let raw = std::fs::read_to_string(settings_path).map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound {
            HooksInstallError::MissingSettings {
                path: settings_path.to_path_buf(),
            }
        } else {
            HooksInstallError::ReadSettings {
                path: settings_path.to_path_buf(),
                source,
            }
        }
    })?;

    if raw.trim().is_empty() {
        return Err(HooksInstallError::EmptySettings {
            path: settings_path.to_path_buf(),
        });
    }

    let mut settings: Value =
        serde_json::from_str(&raw).map_err(|source| HooksInstallError::InvalidSettingsJson {
            path: settings_path.to_path_buf(),
            source,
        })?;

    if !settings.is_object() {
        return Err(HooksInstallError::NotAnObject {
            path: settings_path.to_path_buf(),
        });
    }

    let (hooks_added, hooks_already_present) =
        merge_user_hooks(&mut settings, xdg_hooks_dir, settings_path)?;

    let mut backup_path = None;
    if !dry_run {
        deploy_hooks(xdg_hooks_dir)?;

        let bpath = backup_path_for(settings_path, backup_suffix);
        std::fs::write(&bpath, &raw).map_err(|source| HooksInstallError::BackupWrite {
            path: bpath.clone(),
            source,
        })?;
        backup_path = Some(bpath);

        let mut out = serde_json::to_string_pretty(&settings).expect("Value always serializes");
        out.push('\n');
        std::fs::write(settings_path, out).map_err(|source| HooksInstallError::WriteSettings {
            path: settings_path.to_path_buf(),
            source,
        })?;
    }

    Ok(InstallReport {
        scripts_copied,
        scripts_already_present,
        hooks_added,
        hooks_already_present,
        backup_path,
        dry_run,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_settings(dir: &Path, contents: &str) -> PathBuf {
        let path = dir.join("settings.json");
        std::fs::write(&path, contents).expect("write settings");
        path
    }

    // --- path resolution ---

    #[test]
    fn user_hooks_dir_prefers_xdg_data_home() {
        let home = PathBuf::from("/home/joe");
        let xdg = PathBuf::from("/custom/data");
        let got = user_hooks_dir(Some(&xdg), &home);
        assert_eq!(got, PathBuf::from("/custom/data/tskmstr/hooks"));
    }

    #[test]
    fn user_hooks_dir_falls_back_to_home_local_share() {
        let home = PathBuf::from("/home/joe");
        let got = user_hooks_dir(None, &home);
        assert_eq!(got, PathBuf::from("/home/joe/.local/share/tskmstr/hooks"));
    }

    #[test]
    fn user_settings_path_prefers_claude_config_dir() {
        let home = PathBuf::from("/home/joe");
        let cfg = PathBuf::from("/custom/claude-config");
        let got = user_settings_path(Some(&cfg), &home);
        assert_eq!(got, PathBuf::from("/custom/claude-config/settings.json"));
    }

    #[test]
    fn user_settings_path_falls_back_to_home_dot_claude() {
        let home = PathBuf::from("/home/joe");
        let got = user_settings_path(None, &home);
        assert_eq!(got, PathBuf::from("/home/joe/.claude/settings.json"));
    }

    // --- malformed-input safety ---

    #[test]
    fn install_fails_clearly_when_settings_file_is_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hooks_dir = dir.path().join("hooks");
        let settings_path = dir.path().join("settings.json");

        let err = install_user_hooks(&hooks_dir, &settings_path, "20260101-000000", true)
            .expect_err("missing settings file must error");
        assert!(matches!(err, HooksInstallError::MissingSettings { .. }));
        assert!(!settings_path.exists());
    }

    #[test]
    fn install_fails_clearly_on_empty_settings_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hooks_dir = dir.path().join("hooks");
        let settings_path = write_settings(dir.path(), "");

        let err = install_user_hooks(&hooks_dir, &settings_path, "20260101-000000", true)
            .expect_err("empty settings file must error");
        assert!(matches!(err, HooksInstallError::EmptySettings { .. }));
    }

    #[test]
    fn install_fails_clearly_on_unparseable_settings_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hooks_dir = dir.path().join("hooks");
        let settings_path = write_settings(dir.path(), "{ not json");

        let err = install_user_hooks(&hooks_dir, &settings_path, "20260101-000000", true)
            .expect_err("unparseable settings file must error");
        assert!(matches!(err, HooksInstallError::InvalidSettingsJson { .. }));
    }

    #[test]
    fn install_fails_clearly_when_top_level_is_not_an_object() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hooks_dir = dir.path().join("hooks");
        let settings_path = write_settings(dir.path(), "[1, 2, 3]");

        let err = install_user_hooks(&hooks_dir, &settings_path, "20260101-000000", true)
            .expect_err("non-object top level must error");
        assert!(matches!(err, HooksInstallError::NotAnObject { .. }));
    }

    // --- dry run touches nothing ---

    #[test]
    fn dry_run_writes_no_files_at_all() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hooks_dir = dir.path().join("hooks");
        let original = r#"{"hooks":{}}"#;
        let settings_path = write_settings(dir.path(), original);

        let report = install_user_hooks(&hooks_dir, &settings_path, "20260101-000000", true)
            .expect("dry run should succeed");

        assert!(report.dry_run);
        assert!(report.backup_path.is_none());
        assert!(!hooks_dir.exists(), "dry run must not create the hooks dir");
        let on_disk = std::fs::read_to_string(&settings_path).expect("read back");
        assert_eq!(on_disk, original, "dry run must not touch settings.json");
        assert!(
            dir.path()
                .read_dir()
                .expect("read dir")
                .filter_map(|e| e.ok())
                .all(|e| e.file_name() == "settings.json"),
            "dry run must not create a backup file"
        );
    }

    #[test]
    fn dry_run_reports_what_would_be_added() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hooks_dir = dir.path().join("hooks");
        let settings_path = write_settings(dir.path(), r#"{"hooks":{}}"#);

        let report = install_user_hooks(&hooks_dir, &settings_path, "20260101-000000", true)
            .expect("dry run should succeed");

        assert_eq!(report.hooks_added.len(), 3);
        assert!(
            report
                .hooks_added
                .contains(&"Stop -> tm-usage.sh".to_string())
        );
        assert!(
            report
                .hooks_added
                .contains(&"SubagentStop -> tm-usage.sh".to_string())
        );
        assert!(
            report
                .hooks_added
                .contains(&"SessionEnd -> tm-session-end.sh".to_string())
        );
        assert!(report.hooks_already_present.is_empty());
        assert_eq!(report.scripts_copied.len(), hook_scripts().len());
        assert!(report.scripts_already_present.is_empty());
    }

    // --- real install: additive, idempotent, backed up ---

    #[test]
    fn install_copies_every_hook_script_byte_identical() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hooks_dir = dir.path().join("hooks");
        let settings_path = write_settings(dir.path(), r#"{"hooks":{}}"#);

        install_user_hooks(&hooks_dir, &settings_path, "20260101-000000", false)
            .expect("install should succeed");

        for (name, contents) in hook_scripts() {
            let on_disk =
                std::fs::read_to_string(hooks_dir.join(name)).expect("read deployed script");
            assert_eq!(&on_disk, contents, "{name} not byte-identical");
        }
    }

    #[test]
    fn install_adds_exactly_the_three_documented_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hooks_dir = dir.path().join("hooks");
        let settings_path = write_settings(dir.path(), r#"{"hooks":{}}"#);

        install_user_hooks(&hooks_dir, &settings_path, "20260101-000000", false)
            .expect("install should succeed");

        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).expect("read"))
                .expect("parse");

        let stop = written["hooks"]["Stop"].as_array().expect("Stop array");
        assert_eq!(stop.len(), 1);
        assert_eq!(
            stop[0]["hooks"][0]["command"],
            hooks_dir.join("tm-usage.sh").to_string_lossy().into_owned()
        );
        assert_eq!(stop[0]["hooks"][0]["timeout"], 30);

        let subagent_stop = written["hooks"]["SubagentStop"]
            .as_array()
            .expect("SubagentStop array");
        assert_eq!(subagent_stop.len(), 1);
        assert_eq!(
            subagent_stop[0]["hooks"][0]["command"],
            hooks_dir.join("tm-usage.sh").to_string_lossy().into_owned()
        );

        let session_end = written["hooks"]["SessionEnd"]
            .as_array()
            .expect("SessionEnd array");
        assert_eq!(session_end.len(), 1);
        assert_eq!(
            session_end[0]["hooks"][0]["command"],
            hooks_dir
                .join("tm-session-end.sh")
                .to_string_lossy()
                .into_owned()
        );
    }

    #[test]
    fn install_preserves_unrelated_existing_hook_entries_verbatim() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hooks_dir = dir.path().join("hooks");
        let original = r#"{
            "permissions": { "allow": ["Bash"] },
            "model": "claude-fable-5[1m]",
            "hooks": {
                "Stop": [
                    { "hooks": [ { "type": "command", "command": "/Users/joe/.claude/hooks/peon-ping/peon.sh", "timeout": 10, "async": true } ] }
                ],
                "SessionEnd": [
                    { "matcher": "", "hooks": [ { "type": "command", "command": "/some/other/tool.sh" } ] }
                ]
            }
        }"#;
        let settings_path = write_settings(dir.path(), original);

        install_user_hooks(&hooks_dir, &settings_path, "20260101-000000", false)
            .expect("install should succeed");

        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).expect("read"))
                .expect("parse");

        assert_eq!(written["permissions"]["allow"][0], "Bash");
        assert_eq!(written["model"], "claude-fable-5[1m]");

        let stop = written["hooks"]["Stop"].as_array().expect("Stop array");
        assert_eq!(
            stop.len(),
            2,
            "existing Stop entry must survive alongside the new one"
        );
        assert_eq!(
            stop[0]["hooks"][0]["command"],
            "/Users/joe/.claude/hooks/peon-ping/peon.sh"
        );

        let session_end = written["hooks"]["SessionEnd"]
            .as_array()
            .expect("SessionEnd array");
        assert_eq!(session_end.len(), 2);
        assert_eq!(session_end[0]["hooks"][0]["command"], "/some/other/tool.sh");
    }

    #[test]
    fn install_writes_a_timestamped_backup_with_original_contents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hooks_dir = dir.path().join("hooks");
        let original = r#"{"hooks":{}}"#;
        let settings_path = write_settings(dir.path(), original);

        let report = install_user_hooks(&hooks_dir, &settings_path, "20260101-093000", false)
            .expect("install should succeed");

        let backup_path = report.backup_path.expect("backup path set");
        assert_eq!(
            backup_path.file_name().unwrap().to_string_lossy(),
            "settings.json.bak-20260101-093000"
        );
        let backed_up = std::fs::read_to_string(&backup_path).expect("read backup");
        assert_eq!(backed_up, original);
    }

    #[test]
    fn install_is_idempotent_on_a_second_run() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hooks_dir = dir.path().join("hooks");
        let settings_path = write_settings(dir.path(), r#"{"hooks":{}}"#);

        install_user_hooks(&hooks_dir, &settings_path, "20260101-093000", false)
            .expect("first install");
        let second = install_user_hooks(&hooks_dir, &settings_path, "20260101-094500", false)
            .expect("second install");

        assert!(second.hooks_added.is_empty());
        assert_eq!(second.hooks_already_present.len(), 3);
        assert!(second.scripts_copied.is_empty());
        assert_eq!(second.scripts_already_present.len(), hook_scripts().len());

        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).expect("read"))
                .expect("parse");
        assert_eq!(written["hooks"]["Stop"].as_array().unwrap().len(), 1);
        assert_eq!(
            written["hooks"]["SubagentStop"].as_array().unwrap().len(),
            1
        );
        assert_eq!(written["hooks"]["SessionEnd"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn install_creates_subagent_stop_key_when_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hooks_dir = dir.path().join("hooks");
        // Mirrors the real ~/.claude/settings.json shape observed in
        // practice: Stop and SessionEnd exist, SubagentStop does not.
        let settings_path = write_settings(dir.path(), r#"{"hooks":{"Stop":[],"SessionEnd":[]}}"#);

        install_user_hooks(&hooks_dir, &settings_path, "20260101-093000", false)
            .expect("install should succeed");

        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).expect("read"))
                .expect("parse");
        assert!(written["hooks"]["SubagentStop"].is_array());
    }

    // --- the guard-delegate.sh guardrail, as an actual test ---

    #[test]
    fn never_wires_guard_delegate_at_user_level() {
        for (_, script, _) in USER_HOOK_WIRING {
            assert_ne!(*script, "guard-delegate.sh");
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let hooks_dir = dir.path().join("hooks");
        let settings_path = write_settings(dir.path(), r#"{"hooks":{}}"#);

        install_user_hooks(&hooks_dir, &settings_path, "20260101-093000", false)
            .expect("install should succeed");

        let written = std::fs::read_to_string(&settings_path).expect("read");
        assert!(
            !written.contains("guard-delegate"),
            "guard-delegate.sh must never appear in a user-level settings.json"
        );

        // Also excluded: the other lane-only scripts, none of which close
        // the interactive-session cost gap and all of which add
        // per-tool-call overhead if wired at user level.
        for excluded in [
            "tm-event.sh",
            "tm-checklist.sh",
            "tm-tasklist.sh",
            "tm-session-state.sh",
        ] {
            assert!(
                !written.contains(excluded),
                "{excluded} must not be wired at user level"
            );
        }
    }
}
